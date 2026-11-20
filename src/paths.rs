//! Filesystem layout, mirroring the `CONF_*` / `DATA_*` / `VAR_*` tables in `src/common.c`.
//! All paths are relative to the server root, which the C daemon derives from its own
//! executable location (`main.c:217-229`).

pub const CONF_BBSD: &str = "conf/bbsd.conf";
pub const CONF_MENU: &str = "var/menu_merged.conf";
pub const CONF_BBSNET: &str = "conf/bbsnet.conf";
pub const CONF_BWF: &str = "conf/badwords.conf";
pub const CONF_TOP10_MENU: &str = "var/bbs_top_menu.conf";
pub const CONF_CA_CERT: &str = "conf/ca_cert.pem";

pub const SSH_HOST_RSA_KEY: &str = "conf/ssh_host_rsa_key";
pub const SSH_HOST_ED25519_KEY: &str = "conf/ssh_host_ed25519_key";
pub const SSH_HOST_ECDSA_KEY: &str = "conf/ssh_host_ecdsa_key";
pub const SSH_KNOWN_HOSTS: &str = "var/known_hosts";

pub const LOG_FILE_INFO: &str = "log/bbsd.log";
pub const LOG_FILE_ERROR: &str = "log/error.log";

pub const DATA_WELCOME: &str = "data/welcome.txt";
pub const DATA_REGISTER: &str = "data/register.txt";
pub const DATA_GOODBYE: &str = "data/goodbye.txt";
pub const DATA_EULA: &str = "data/eula.txt";
pub const DATA_LICENSE: &str = "data/license.txt";
pub const DATA_COPYRIGHT: &str = "data/copyright.txt";
pub const DATA_VERSION: &str = "data/version.txt";
pub const DATA_LOGIN_ERROR: &str = "data/login_error.txt";
pub const DATA_ACTIVE_BOARD: &str = "data/active_board.txt";
pub const DATA_READ_HELP: &str = "data/read_help.txt";
pub const DATA_EDITOR_HELP: &str = "data/editor_help.txt";
pub const VAR_BBS_TOP: &str = "var/bbs_top.txt";

pub const VAR_ARTICLE_CACHE_DIR: &str = "var/articles/";
pub const VAR_GEN_EX_MENU_DIR: &str = "var/gen_ex/";
pub const VAR_SECTION_AID_LOC_DIR: &str = "var/section_aid_loc/";

/// Files preloaded into memory at startup, in the C order (`common.c:70-82`).
pub const DATA_FILES_STARTUP: [&str; 12] = [
    DATA_WELCOME,
    DATA_REGISTER,
    DATA_GOODBYE,
    DATA_EULA,
    DATA_LICENSE,
    DATA_COPYRIGHT,
    DATA_VERSION,
    DATA_LOGIN_ERROR,
    DATA_ACTIVE_BOARD,
    DATA_READ_HELP,
    DATA_EDITOR_HELP,
    VAR_BBS_TOP,
];

/// `FILE_PATH_LEN` (`include/common.h:19`).
pub const FILE_PATH_LEN: usize = 4096;

/// Screen geometry. The C server hard-codes this and never calls `TIOCGWINSZ`
/// for the telnet path, so the port keeps it fixed as well.
pub const SCREEN_ROWS: usize = 24;
pub const SCREEN_COLS: usize = 80;
pub const LINE_BUFFER_LEN: usize = 1024;
pub const TAB_SIZE: usize = 4;

pub const MAX_CLIENT_LIMIT: usize = 2000;
pub const MAX_CLIENT_PER_IP_LIMIT: usize = 1000;

/// `MAX_SPLIT_FILE_LINES` from `include/str_process.h`.
pub const MAX_SPLIT_FILE_LINES: usize = 65536;

/// The `bbs_const_t` limits of `include/bbs.h` that size a buffer or a truncation.
pub const USERNAME_MAX_LEN: usize = 12;
pub const SECTION_NAME_MAX_LEN: usize = 20;
pub const SECTION_TITLE_MAX_LEN: usize = 40;
pub const NICKNAME_MAX_LEN: usize = 40;
pub const USER_TZ_MAX_LEN: usize = 50;
pub const CURRENT_ACTION_MAX_LEN: usize = 20;
pub const SESSION_ID_LEN: usize = 32;
pub const IP_ADDR_LEN: usize = 50;
pub const MAX_USER_IDLE_TIME: u64 = 600;

/// `BBS_user_off_line` (`bbs.c:43`) — the idle window the online list is filtered by. It is
/// deliberately wider than `MAX_USER_IDLE_TIME`, which only expires a session's own lock.
pub const USER_OFF_LINE_SECS: i64 = 900;

/// `BBS_user_limit_per_page` (`include/user_list.h:18`).
pub const USER_LIMIT_PER_PAGE: i32 = 20;

/// `BBS_user_intro_max_len` / `BBS_user_intro_max_line` (`include/user_list.h:22-23`): the
/// size of the `user_info.intro` buffer and the line limit the editor enforces on it.
pub const USER_INTRO_MAX_LEN: usize = 4096;
pub const USER_INTRO_MAX_LINE: usize = 10;

/// `char master_list[(BBS_username_max_len + 1) * 3 + 1]` (`section_list.h:64`): one run of
/// up to three masters, the NUL excluded.
pub const MASTER_LIST_BYTES: usize = (USERNAME_MAX_LEN + 1) * 3;

/// `section_list_constant_t` (`include/section_list.h:19`).
pub const ARTICLE_TITLE_MAX_LEN: usize = 160;
pub const ARTICLE_LIMIT_PER_SECTION: i32 = 50000;
pub const ARTICLE_LIMIT_PER_PAGE: i32 = 20;
pub const ONTOP_ARTICLE_LIMIT: usize = 10;

/// `BBS_article_limit_per_section / BBS_article_limit_per_page`, the capacity of the
/// `p_page_first_article` index.
pub const PAGE_INDEX_CAPACITY: usize = (ARTICLE_LIMIT_PER_SECTION / ARTICLE_LIMIT_PER_PAGE) as usize;

/// `BBS_max_section`, `BBS_max_user_count` and `BBS_max_user_online_count`: the sizes the C
/// pools are `mmap`ed with.
pub const MAX_SECTION: usize = 200;
pub const MAX_USER_COUNT: usize = 100000;
pub const MAX_USER_ONLINE_COUNT: usize = 10000;
