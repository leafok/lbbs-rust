# lbbs-rust —— 用 Rust 重写 LBBS

> 本文档的默认版本是英文：[README.md](README.md)。
> 这里是它的简体中文翻译，内容与英文版一致。

这份文档的目标是：**让一个没用过 BBS、也不熟悉 Rust 的人，能明白这个项目在做什么。**
各章节是层层递进的，从头往下读就可以。

---

## 1. 这是什么

### 1.1 原程序（LBBS）

| 项目 | 内容 |
| --- | --- |
| 名称 | LBBS (Leaflet BBS Daemon) |
| 版本 | 1.8.0 |
| 语言 | C（`src/*.c` 加 `include/*.h` 共 37,364 行：服务器 27,427 行，其余是它自己的测试程序） |
| 许可证 | GPL-3.0-or-later |
| 原作者 | Leaflet &lt;leaflet@leafok.com&gt; |
| 与本项目同级的参考副本 | `../lbbs` —— 只读；**我们不会修改它** |

LBBS 是一个**用纯文本终端访问的论坛（BBS）服务器**。用户通过 telnet 或 SSH 连上来，按菜单键看版块、发帖。
没有鼠标，也没有图片：画面完全由文字加上**终端控制序列**（形如 `ESC[...H` 的字符串，用来设置颜色和光标位置）画出来。

另一个关键点：**LBBS 并不自己保管数据。** 帖子正文和附件是磁盘上的文件，而用户、版块、帖子的索引存在一个
**MySQL 数据库**里。本站的**网页版论坛**读的是同一个数据库。所以 LBBS 实际上是 *那个网页版论坛的终端前端*，
这带来一条硬约束：**不能擅自改数据库结构。**

C 原程序还在继续开发，所以一处代码引用只有说清修订才有意义。Rust 源码里出现的每一个 `file.c:NNN`
引用都指向 `../lbbs` 里**已提交**的某个修订，从不指向它的工作区——那是作者自己的目录，会随他动，也常常带着
没提交的改动。`git -C ../lbbs show HEAD:src/<文件>`（C 的测试程序在 `…:tests/test_<名字>.c`，游戏在
`…:utils/game/chicken.c`）打印出来的，正是那条注释所描述的内容。上游的发布标签并不全在 `main` 的历史上，
所以参考端按修订来称呼，而不按标签。

### 1.2 本项目（lbbs-rust）

我们在把那个 C 服务器用 Rust 重写。

- 代码结构（模块怎么划分、函数干什么）刻意与原 C 一一对应。
- 使用**同一个数据库**，读写**同样格式的配置文件和数据文件**，因此网页版照旧工作，不需要任何数据迁移。
- 用户能看见的细节——输出、按键、UTF-8/GBK 编码——尽可能做到和原来一致。
- **服务器本体已经就位**：`main.rs` 负责启动、加载数据、拉起两个监听端口，一次连接会按 C 的顺序走完
  编码选择、欢迎画面、登录、上位菜单。游客可以进版块、翻页、读一篇帖子；写入路径（发帖、回复、修改、
  删除，以及全屏编辑器）也已经接在和 C 相同的权限判断后面。
  [第 6 节](#6-哪些能用了哪些还没有)如实列出还缺的部分。

> **常见误解**：“`cargo run` 就能跑起一个论坛”——**现在可以了**，游客能进到上位菜单，
> 也能进版块读帖子。

---

## 2. 小词典

本文档和源码里反复出现的词。

| 术语 | 含义 |
| --- | --- |
| BBS | 电子公告板。用户连上来读写帖子 |
| telnet | 用裸 TCP 接终端的老办法。LBBS 自己会说一点 telnet 协议（回显和 Go-Ahead 的协商） |
| SSH | 加密连接方式。LBBS 实现了服务端；用户用 `ssh -p 2322` 连进来 |
| 会话（session） | 一次连接：从第一个提示开始到断开为止 |
| 版块 / section | 论坛的分类（比如“诗词”“电脑”）。数据在 `section_config` 表，按 `section_class` 分组，其 ID 称为 **SID** |
| 帖子 / article | 版块中的一条投稿。其 ID 称为 **AID**，对应 `bbs` 索引表的一行，正文另存在 `bbs_content` |
| 用户 | 登录的人，ID 称为 **UID**，数据分在 `user_list`（凭据、标志）和 `user_pubinfo`（资料）两张表 |
| 权限（privilege） | 表示此人能否阅读、发帖、管理版块的若干比特。C 里放在全局变量 `BBS_priv` |
| 菜单 | `menu.conf` 里的条目表，决定按哪个键触发哪个功能 |
| LML | LBBS Markup Language，论坛自己的颜色与坐标标记，排版帖子时使用 |
| badwords | 违禁词表，发帖时被自动替换成 `*` |
| AID 位置（`section_aid_loc`） | 记录“这个版块最后读到哪一篇”之类状态的文件 |
| GBK / UTF-8 | 中文编码。连接时让用户选一种 |
| 网页版论坛 | 与本程序共用同一个数据库、同一批文件的论坛网页端 |

---

## 3. 它是怎么跑起来的（C 版的设计）

先理解原程序的设计，后面每个移植决策的理由就都串起来了。

```
                  ┌──────────────────────────────────────────────┐
 终端 ── telnet ──►  bbsd（父进程）                               │
 终端 ── ssh ─────►  等待连接 → fork() 出子进程                   │
                  └───────────────┬──────────────────────────────┘
                                  │ 一个子进程 = 一次会话
                                  │  · 自己的状态放在进程全局变量 BBS_* 里
                                  │  · 通过 libmysqlclient 直接对 MySQL 发 SQL
                                  │  · 版块/帖子/用户从共享内存里读
                                  ▼
                        MySQL（和网页版论坛同一个库）
                        var/articles/<CID>、data/*.txt、var/*.conf
```

这套设计有两点最重要：

1. **一次连接 = 一个 fork 出来的进程。** 所以 C 可以放心使用全局变量——代价是每个连接都要建立一条 MySQL 连接。
2. **版块、帖子、用户清单常驻在共享内存里**，而且刷新是**增量的**：只重读上一次扫描以来变化过的行。刷新靠的是
   **定时器**而不是信号——loader 进程按 `BBS_section_list_load_interval`（`bbs.c:38`，1 秒）循环，`SIGUSR1`
   只用来重新打开日志文件（父进程干脆把它忽略了）。

---

## 4. Rust 版的四个设计决策

每一条都是刻意选的，也都有理由。

### 4.1 单进程 + Tokio 异步任务（不再 fork）

`fork()`、POSIX 共享内存和 System V 信号量与 Rust 的安全模型很冲突，除了多出裸 `libc` 调用没有别的好处。
整个服务器改为一个进程内运行 **Tokio** 任务。

- C 的“每进程全局变量”→ [`Session`](src/session.rs) 结构体的字段。
- C 的“共享内存”→ 进程内的 [`Arc<RwLock<...>>`](src/state.rs)。

> 第一次接触异步？`async fn` 就是“需要等待”的函数。每写一个 `.await` 就把线程让给别的任务，所以一个线程
> 也能同时伺候大量连接。最容易踩的点是：**没有 await 的代码并不代表已经执行完**，这是它和同步代码最大的差别。

### 4.2 数据库与文件格式保持不变（与网页版共存）

`conf/bbsd.conf`、`conf/menu.conf`、`var/menu_merged.conf`、`var/bbs_top_menu.conf`、`data/*.txt`、
`var/articles/<CID>` 都按 C 的方式原样读写。认证沿用数据库侧计算 `SHA2(?, 256)` 的原式，
因此在网页端注册的账号可以直接从这里登录。

### 4.3 分阶段交付，先做核心闭环

| 阶段 | 内容 | 状态 |
| --- | --- | --- |
| Phase 1a | 守护进程启动、两个监听端口、连接数限制、日志、信号、编码选择、登录、菜单 | **完成**，并已对真实数据库做过冒烟测试 |
| Phase 1b | 版块列表、帖子列表、阅读（分页器）、发帖/回复、违禁词接入菜单 | **完成**，含写入路径与全屏编辑器 |
| Phase 2 | 精华区树、统计、用户资料编辑器、GBK 完善、SSH 窗口尺寸 | **大体完成**：精华区树和它的生成工具、资料页“发文”数字背后的那个计数器、两段资料文字的编辑器都已做完。剩下的只有 GBK 边界情况与 SSH 窗口尺寸（见 §6.3） |

每个阶段结束时都必须**编译通过、测试通过**。我们不靠“先堆一堆跑不起来的代码”前进。

### 4.4 SSH 用 russh，主机密钥沿用现有文件

复用 `conf/ssh_host_*` 里的私钥，用 `russh` crate 的服务端实现去调用与 telnet 路径完全相同的会话代码。
思路是只替换传输层，不改变 C 版通过 libssh 实现的行为。

---

## 5. 怎么读现在的源码

规则是 **一个 Rust 模块 ↔ 一个 C 文件**。每个模块开头的文档注释写明了它移植自哪个 C 文件的哪些行，
随后是一节 **`# Deviations from C`（与 C 不同的地方）**。

这个结构是刻意做的：懂 C 就一定能找到对应的 Rust。反过来，如果你觉得“C 里好好的，Rust 却不一样”，
请先读 deviations 那节——理由都写在里面。

### 5.1 模块对照表

| Rust 文件 | 行数 | 对应 C | 职责 | 状态 |
| --- | ---:| --- | --- | --- |
| [src/paths.rs](src/paths.rs) | 118 | `include/bbs.h` 等 | 屏幕尺寸、各字段长度、路径名等常量 | 完成 |
| [src/config.rs](src/config.rs) | 394 | `src/init.c` | 读取 `conf/bbsd.conf` | 完成 |
| [src/text.rs](src/text.rs) | 635 | `src/str_process.c` | 显示宽度（全角=2 列）与折行计算 | 完成 |
| [src/term/mod.rs](src/term/mod.rs) | 892 | `src/io.c` | 字节管道、GBK 转码、`prints`/`iflush`、`igetch` | 完成 |
| [src/term/key.rs](src/term/key.rs) | 524 | `src/io.c` | 把 ESC 序列翻译成按键码 | 完成 |
| [src/screen.rs](src/screen.rs) | 1151 | `src/screen.c` | 清屏、回显接管 `set_input_echo`、行输入 `str_input`、分页器 `display_data` | 完成 |
| [src/db.rs](src/db.rs) | 1581 | `src/database.c` 等 | 经 sqlx 访问 MySQL | 完成 |
| [src/board.rs](src/board.rs) | 1602 | `src/section_list_loader.c` | 版块/帖子/用户的内存池 | 完成 |
| [src/browse.rs](src/browse.rs) | 3371 | `src/section_list_display.c`、`src/section_list_loader.c`、`src/menu_proc.c`（两个精华区入口） | 帖子列表、列表按键、阅读帖子、阅读位置文件，以及版块精华区树的浏览与树里读一篇帖子 | 完成 |
| [src/editor.rs](src/editor.rs) | 3292 | `src/editor.c` | 全屏编辑器：行列、窗口、状态行、按键 | 完成 |
| [src/post.rs](src/post.rs) | 1677 | `src/article_post.c`、`src/article_del.c`、`src/article_op.c` | 发帖、回复、修改、删除、精华标记、一篇帖子的网页链接，以及各个写入动作自己的事务 | 完成 |
| [src/viewlog.rs](src/viewlog.rs) | 369 | `src/article_view_log.c` | 记录该用户读过哪些帖子 | 完成 |
| [src/favorlog.rs](src/favorlog.rs) | 452 | `src/article_favor.c` | 记录该用户收藏了哪些帖子 | 完成 |
| [src/favorlist.rs](src/favorlist.rs) | 785 | `src/article_favor_display.c`、`src/article_favor.c`（取页）、`src/menu_proc.c`（`FAVOR_TOPIC` 菜单项） | 我收藏的主题列表：两个名称栏目、就地取消收藏，以及每行通向的看板 | 完成 |
| [src/roster.rs](src/roster.rs) | 931 | `src/user_list_loader.c`、`src/user_stat.c` | 在线用户列表与发帖数统计 | 完成 |
| [src/userinfo.rs](src/userinfo.rs) | 966 | `src/user_info_display.c`、`src/user_list_display.c`（查找）、`src/bbs.c`（等级与星座表） | 一个用户的资料页，以及通向它的用户名查找 | 完成 |
| [src/user_info_update.rs](src/user_info_update.rs) | 677 | `src/user_info_update.c`、`src/menu_proc.c`（那两个资料条目） | 说明档与签名档两个编辑器：取 `user_pubinfo` 的一列走一遍编辑器，过敏感词、数行数，再写回去 | 完成 |
| [src/userlist.rs](src/userlist.rs) | 1053 | `src/user_list_display.c`、`src/menu_proc.c`（两个花名册入口） | 线上使用者与已注册用户两页：标题、栏目、翻页，以及在两页之间走动的按键 | 完成 |
| [src/loader.rs](src/loader.rs) | 1689 | `src/section_list_loader.c` | 增量刷新任务、它顺带写出的文章缓存文件，以及每个版块那棵精华区菜单 | 完成 |
| [src/cache.rs](src/cache.rs) | 1849 | `src/article_cache.c` | 渲染与读取 `var/articles/<CID>` 及其行偏移表 | 完成 |
| [src/data_file_mgr.rs](src/data_file_mgr.rs) | 456 | `src/data_file_mgr.c`、`src/bbs_cmd.c`（`DATAMGR` 那条） | `@DATAMGR` 列表：按编号选一份站点固定文字，编辑、预览，再把结果同时写回安装的那份和它的 `.user` 副本 | 完成 |
| [src/menu.rs](src/menu.rs) | 2744 | `src/menu.c` | `menu.conf` 解析与页面跳转 | 解析/跳转完成，功能本体未完成 |
| [src/lml.rs](src/lml.rs) | 720 | `src/lml.c` | LML 标记渲染 | 完成 |
| [src/bwf.rs](src/bwf.rs) | 242 | `src/trie_dict.c` 等 | 违禁词匹配与遮蔽 | 完成 |
| [src/ipmask.rs](src/ipmask.rs) | 109 | `src/ip_mask.c` | IP 地址掩码（`202.38.64.x`） | 完成 |
| [src/privilege.rs](src/privilege.rs) | 561 | `src/user_priv.c` | 权限位与权限等级计算 | 完成 |
| [src/session.rs](src/session.rs) | 619 | 各 `BBS_*` 全局变量 | 一次会话的状态 + 在线记录 | 完成 |
| [src/login.rs](src/login.rs) | 585 | `src/login.c` | 登录、认证、用户协议（EULA） | 完成 |
| [src/bbsnet.rs](src/bbsnet.rs) | 1852 | `src/bbs_net.c`、`src/menu_proc.c`（`@mod:` 分发） | 站点表、穿梭网格、对外连接与双向转发 | 完成 |
| [src/chicken.rs](src/chicken.rs) | 2405 | `utils/game/chicken.c` | 电子小鸡：一行十四个数字加一个名字、取名那一问、对一只小鸡能做的九件事，以及孵出下一颗蛋的那次死亡 | 完成 |
| [src/money.rs](src/money.rs) | 243 | `utils/game/money.c` | 糖糖：会话缓存的余额、到上限就停住的存钱，以及要么全给要么不给的取钱 | 完成 |
| [src/bbs.rs](src/bbs.rs) | 1127 | `src/bbs_main.c`、`src/bbs_cmd.c` | 会话自身的执行顺序（含回显接管）、`bbs_center` | 菜单循环完成，`conf/menu.conf` 绑定的每个动作都有处理函数（收藏目录那个是菜单过滤条件，不是动作） |
| [src/net.rs](src/net.rs) | 469 | `src/net_server.c` | accept、连接数限制、优雅退出 | 完成 |
| [src/ssh.rs](src/ssh.rs) | 504 | `src/net_server.c`（libssh 那一半） | 宿主密钥、密码认证、会话通道 | 完成 |
| [src/main.rs](src/main.rs) | 594 | `src/main.c`、`src/net_server.c` | 启动顺序、信号处理、`SIGHUP` 重载、退出 | 完成 |
| [src/batch.rs](src/batch.rs) | 385 | `utils/lib/db_open.inc.php` + 三个脚本里反复出现的那几行 | 一个生成工具的完整一生：参数、服务器根目录、`conf/bbsd.conf`、数据库，以及脚本自己的 stdout 文字与退出码 | 完成 |
| [src/section_list.rs](src/section_list.rs) | 149 | `utils/lib/section_list.inc.php` | 生成工具要走的那棵分类/版块树，附带调用方自己的每版数据 | 完成 |
| [src/gen_top.rs](src/gen_top.rs) | 306 | `utils/bin/gen_top.php` | `var/bbs_top.txt` 与 `var/bbs_top_menu.conf`：本周十篇最热帖子 | 完成 |
| [src/gen_section_menu.rs](src/gen_section_menu.rs) | 691 | `utils/bin/gen_section_menu.php` | `var/menu_merged.conf`：手写的 `conf/menu.conf` 后面接上六个生成的菜单块 | 完成 |
| [src/gen_ex_list.rs](src/gen_ex_list.rs) | 424 | `utils/bin/gen_ex_list.php` | 被守护进程标了记的每个版块，各写出一个 `var/gen_ex/<SID>` 精华区树 | 完成 |
| [src/bin/gen_top.rs](src/bin/gen_top.rs)、[gen_section_menu.rs](src/bin/gen_section_menu.rs)、[gen_ex_list.rs](src/bin/gen_ex_list.rs) | 30 | `php utils/bin/*.php` 那几行 crontab | 三个可执行文件：调用三个生成工具，把脚本的退出码原样交回给 shell | 完成 |

另有 [src/daemon.rs](src/daemon.rs)（488 行，命令行与后台化）、[src/systemd.rs](src/systemd.rs)（220 行，不依赖 libsystemd 的 `sd_notify`）、[src/log.rs](src/log.rs)（542 行，两个日志文件）、[src/state.rs](src/state.rs)（410 行，加载器发布、会话读取的共享状态）。

合计 40,931 行（含测试，测试就写在被测模块里）。共 609 个测试：库 603 个、可执行文件 6 个。其中 589 个只读代码
仓库里的东西，另外 14 个默认跳过，因为它们要连真实数据库、读真实生成的文件。

> 数字会随开发变化。以 `cargo test --lib` 的输出为准。

---

## 6. 哪些能用了，哪些还没有

如实交代进度是这套方法的一部分：说“能用”的都有测试覆盖，跑不起来的东西绝不说成能用。

### 6.1 ✅ 已完成（且有测试）

1. **配置解析**——`conf/bbsd.conf` 的切分方式与 C 完全相同（空格、制表符、CR、LF 为分隔；键名不区分大小写）。
2. **显示宽度计算**——中日韩全角字符算 2 列，控制序列算 0 列，规则与 C 的 `str_process.c` 一致。
3. **按键解析**——`ESC [ A` 被识别为“上箭头”，`ESC [ H`/`ESC [ F` 被识别为 Home 与 End；跨包切断的序列会等
   十分之一秒再接回，序列本身和它后面紧跟的那个键都不会丢；缓冲区里只剩一个孤立的 `ESC` 时仍旧报成裸 `ESC`，
   与 `igetch` 相同。
4. **字符编码**——UTF-8 与 GBK 都支持；GBK 字符在包边界被切断也能正确拼回，`0xff` 控制转义序列也一样。
   `io_buf_conv`（io.c:1142-1176）会把 `0xff` 和紧随的那一个字节留给 `igetch`、不送进转码器，而 telnet 客户端
   应答协商发回的 `IAC` 正是这种字节：以前它们被当成文本转码，在登录行上变成乱码并把后面真正的按键吃掉，
   这就是选了 `(G)GBK` 之后输出不对的原因。
5. **行输入（`str_input`）**与**分页器（`display_data`）**——密码不回显（`NOECHO` 提示打印 C 的
   `*`/`**` 掩码）、按 `q` 退出等按键行为与 C 一致。
6. **MySQL 查询**——凭据校验、版块与帖子索引、在线列表、经验值累计等。
7. **版块/帖子/用户的内存池**（替代共享内存）**以及增量更新逻辑**。
8. **`menu.conf` 的语法解析**、条目光标移动、按权限过滤。
9. **LML 渲染**与**违禁词遮蔽**。
10. **权限模型**（`user_priv.c`）与 **IP 掩码**（`ip_mask.c`）。
11. **登录流程**：三次尝试、`guest` 与 `new` 的特殊处理、用户协议重新确认、防爆破限流（按 /24 网段与按账号），
    以及“版主和管理员必须走 SSH 登录”的规则。[`login`](src/login.rs) 里的离线测试钉住的是用户名与密码的取值形状、
    账号有效期、两个提示语和三次静默失败后断开。限流和 `staff_must_use_ssh` 要先在 `user_err_login_log` 里有
    可数的记录才会启动，离线造不出来；它们用的两条查询在 §7.2 那组只读实机测试里跑。
12. **服务器本体**：`conf/bbsd.conf` → 共享状态 → 加载任务 → 两个监听端口，含 C 的命令行、
    `--foreground`/`--display-log`、在建 runtime 之前完成的 `fork()`、两个日志文件与 `log_restart()`、
    `SIGHUP` 重载，以及先等 15 秒、再给 1 秒的 `SIGTERM` 收尾。[`net`](src/net.rs) 的测试钉住的是两道放行判断；
    真正的收尾和重载是对着跑起来的守护进程手工验证的（§6.2）。周期任务用的就是做完初始加载的那个
    loader——C 那边靠 `fork()` 把游标交给了子进程。`last_article_op_log_mid` 是一次扫描够得着
    `bbs_article_op` 的前提，另起一个空白 loader 的循环只会不停地追加新帖，老帖的改动一条也看不见。
13. **连接数限制**：总量 `bbs_max_client` 与单 IP `bbs_max_client_per_ip`，计数方式与 C 父进程一致，
    日志行也一致。
14. **SSH**：用 russh 复用同一批宿主密钥，只提供密码认证、同样三次尝试、一条会话通道，
    会话里跑的就是 telnet 那条 `bbs_main`。这是唯一一个没有自己的离线测试的入口——核对它靠的是 §6.2 里那条从
    russh 进来的会话——与 C 的两处有意不同（不理会窗口尺寸、不设 60 秒认证计时）写在 [`ssh`](src/ssh.rs) 的开头。
15. **会话开场**，顺序与 C 一致：先用
    [`screen::set_input_echo`](src/screen.rs) 送出
    `IAC WILL ECHO, IAC WILL SGA`，让客户端不再自己回显；随后是两个 5 秒提示、`bbs_info`、
    `data/welcome.txt`、统计行、`var/bbs_top.txt` 与带十秒版块面板的上位菜单。
16. **版块的帖子列表**——分页、最后一页重复的置顶行、光标与每一个移动光标的键、主题的配色、
    `[提示]`/`@`/`N`/`x` 标记、两种日期格式，以及按作者、按标题、按未读帖子的扫描。读帖子走
    `var/articles/<CID>` 和同一个分页器；离开版块时像 C 那样写 `var/section_aid_loc/<UID>`。
17. **每个用户读过哪些帖子、给哪些帖子加过星**，写进网页端读取的那几张表，
    刷新时机也与 C 相同。
18. **写入路径**，由帖子列表上的按键进入：`article_post`、`article_reply`、`article_modify`、`article_del`，
    每一步都走与 C 相同的权限判断，含标题/类别提示、帖子头、`article`/`thread`/`section` 索引更新、
    经验值变动与 `article_log` 行——和一个事务里完成的 C `*_tx` 系列一致。游客按 `^P` 会在任何 SQL 之前
    收到 `您没有权限在本版块发表文章`。
    列表上的 `m` 则是 `article_excerption_set`（`article_op.c:34-280`），也就是精华标记的开关：先 `FOR UPDATE`
    锁出那一行的五列，再改标记；取消时顺带清掉 `gen_ex`/`static` 与 `ex_file`，附件的归属随之转移，作者的
    经验值按原版主题 50、转帖 20、回复 10 计费，取消时同样数额倒扣，最后写进 `'E'`/`'O'` 那条操作记录，
    提交之前再改共享池里的标记——C 提交失败时会把池子的标记退回去了。它一个字符都不画：行首那个标记就是
    全部的回应；过不了 `S_POST | S_MAN_S` 的用户按下去，列表原样重画，没有提示也没有响铃。
    同一个文件的另一个函数 `display_article_meta`（`article_op.c:22-32`）是列表上的 `Ctrl-Q`：清屏，第 3 行写下
    `Web版 文章链接:`，第 4 行写下本站自己的
    `https://<BBS_server>/bbs/view_article.php?id=<AID>#<AID>`，然后等一个键。C 在它返回负值时才记的那条
    `log_error` 永远走不到——这个函数只会返回 `0`——移植过去也就没有需要报告的东西。
19. **全屏编辑器**（`editor.c`）：插入与改写、行与窗口的行列运算、`Ctrl-K`/`Ctrl-Y` 删除、退格合并行、
    宽字符整体删除、`Ctrl-W`/`Ctrl-X` 不重绘直接退出、状态行，以及 `MAX_EDITOR_DATA_LINES` 上限。决定一次
    按键要不要拆行、一次合并要不要并行的那两处判断，还要问这一行的末尾是不是换行符；`Ctrl-K` 也留下那个换行符，
    不再把它一起带走。`Ctrl-Y` 删空一格不留的行时，那一行本身作为一行空行留下，光标仍停在它下面，下一个
    字符也就落在那里，而不是回到第一行的行首。加载器自己也会这么切：塞不进那一块 1,024 字节的文本行在块边界处
    断开，切口留下一个换行符。在一格不留的满行上按回车，这个换行符留在原行里，而不是另起一行，光标于是停在断行
    开出的那一行行首。
    在一格不留的满行上按回车，这个换行符留在原行里，而不是另起一行，光标于是停在断行开出的那一行行首。
20. **生成文章缓存。** [`cache::generate`](src/cache.rs) 与 C 的
    `article_cache_generate` 逐字节一致，加载器在每收录一帖、每重放一个 `'M'` 之后都会调用它
    （`section_list_loader.c:333`、`:550`），所以 `var/articles/<CID>` 会自己出现——帖子落库后一秒之内，与 C 相同。
    参考端自己的那套缓存测试（`tests/test_article_cache.c`）在这里由十个测试复演：写同样的文章，再拿本项目
    自己的一套终端模型把文件读回来对。
21. **两张花名册页面和它们身后的资料页**——`user_list_display(1)` 与 `user_list_display(0)`
    （[`userlist::user_list_display`](src/userlist.rs)），只从主菜单的
    线上使用者、已注册用户两项进来。两页是同一个页面换一面旗子：标题行、栏头、一页 20 行，加上 `k`/`j`/`p`/`n`/
    `↑`/`↓`/`Home`/`End`/`PgUp`/`PgDn`——含 C 那个一键两用的习惯：翻到页尾再按 `Home` 会先定在第一页、接着照样挪
    光标，而 `Enter` 打开光标那一行用户的资料页
    （[`userinfo::user_info_display`](src/userinfo.rs)）再重画本页。
    `s` 在名单里跑用户名查找，`h` 显示阅读帮助，`N` 转到最后一条、`P` 转到第一条。三个时长栏都是把裸秒数交给
    `gmtime` 当成日期来读，所以空闲一小时打的是 `0"`，一年前登录打的是 `1年0天`。
22. **站长的 关闭系统 菜单项**——`shutdown_bbs`（`menu_proc.c:176-193`），连问句一起。答案只有一格宽，
    而那一格一开始就坐着默认的 `N`，所以直接输入会被 `get_data` 响铃拒绝，`Y` 要先把 `N` 删掉才填得进去；
    回车、`Esc`、超时都让系统继续运行，两种结果都由 `REDRAW` 把菜单重新画回问句之上。
23. **站点穿梭**——`bbs_net`（[`bbsnet::bbs_net`](src/bbsnet.rs)），入口是
    特别服务区菜单里的 `【 网络连线 】`，走的还是 C 那条路：`@RunMBEM` 的参数 `N@mod:lib/libbbs.so#bbs_net`
    把入口名写在 `#` 后面，而穿梭无论答回什么，`REDRAW` 都会把菜单重画一次。站点表读自 `conf/bbsnet.conf`，
    字段上限照 C（40、40、253、5、20 字节），`#` 开头是注释，端口先按 `strtol` 读、再验 `1..65535`，
    所以坏掉的一行是整行跳过而不是读半截。网格一行四个站点——`'A'…'Z'` 用完接 `'a'…'z'`，行号 `2+id/4`、
    列号 `5+id%4*20`——下面跟着所选站点的详情块；`↑`/`↓` 一次走一整行，`←`/`→` 走一格。回车进入
    `bbsnet_connect`：十秒预算配着那条反白的进度条，解析—绑定—非阻塞 connect 的顺序与 C 相同，被拒绝的连接
    要等预算走完才报出来（这个怪癖照留），然后 `IPTOS_LOWDELAY`，再按站点自己的字符集双向转码中继；
    标了 `SSH` 的站点只对从 SSH 进来的会话开放，libssh 那一段换用 russh 重写，四种宿主密钥答案和五条提示都在。
    进度条跑动时按下的键会被连接吃掉，与 C 一致；`Ctrl+C`、`Esc` 取消，任何结果都重画整个界面。
24. **我收藏的主题列表**——`article_favor_display`（`article_favor_display.c:296-401`），入口是上位菜单的
    `@FAVOR_TOPIC`（`menu_proc.c:372-380`），那个参数 C 根本不看。每行的格式是 `"  %7d %c %s%*s %s %s"`：
    文章编号、已读标记、`n` 所选的那一栏（发表者 12 格，版块名 20 格）、日期，再加上标题在 59 格里剩下的宽度；
    主题头带 `● `，转帖带 `[转载]`，但列表不像版面那样把重复的 `Re: ` 折叠起来。收藏表先经
    `query_favor_articles`（`article_favor.c:450-518`）读出，开头就是落盘与合并，所以 `-` 取消收藏在重画之前
    已经写定。C 的三个松口都留着：`KEY_NULL` 记一条日志之后和 `Esc` 走同一条路；`UNSET_FAVOR` 没有 `break`，
    直接落进 `CHANGE_PAGE` 的那次刷新；`LOCATE_ARTICLE` 回来只重画第 4-23 行和最底一行，不重画表头，于是刚离开
    的版面把自己的标题留在屏上。取消收藏让记下的 `page_id` 越界时——最后一页的最后一行——列表按 C 的范围检查
    直接退出，而不是退回新的末尾。
25. **三个批量生成工具**——`utils/bin/gen_top.php`、`gen_section_menu.php`、`gen_ex_list.php`，现在分别是
    [`gen_top::run`](src/gen_top.rs)、
    [`gen_section_menu::run`](src/gen_section_menu.rs) 与
    [`gen_ex_list::run`](src/gen_ex_list.rs)，各自有一个 `bin/` 下的
    可执行文件，共用 [`batch`](src/batch.rs) 和
    [`section_list`](src/section_list.rs)。它们读守护进程正在用的那个
    数据库（凭据同样来自 `conf/bbsd.conf`），写出来的也是同一批文件：十大热门的文本与菜单、接到手写
    `conf/menu.conf` 之后的六个生成菜单块，以及每个 `ex_menu_update` 被置起的版块的一棵精华区树。
    PHP 与「按 Rust 的直觉重写」相冲突的地方，一律以 PHP 为准：heredoc 会丢掉结尾那一行的换行，所以一个块
    是一条记录一行、块内没有空行；`str_repeat` 的次数为负是致命错误，它之后的版块就不再写了；标题先量宽度
    再转义，转义撑长的部分不会补回空格；`NULL` 日期打印当前时间；`intval(log10($n))` 数的是浮点的位数，
    `0` 按 `1` 算。
26. **版块的精华区树**——`section_list_ex_dir_display`（`section_list_display.c:1380-1471`），入口有两个：
    帖子列表的 `x` 键（`VIEW_EX_DIR`，`section_list_display.c:1353-1363`）与上位菜单的 `@LIST_EX_SECTION`
    （`menu_proc.c:259-276`），树里的帖子由 `view_ex_article`（`menu_proc.c:218-257`）读出。树就是第 25 条写出的
    `var/gen_ex/<SID>`，由加载器每个版块解析一次——也就是 C 的 `load_section_config_from_db` 里 `update_gen_ex`
    那一半（`section_list_loader.c:162-179`）——再用 `Arc` 共享出去，替掉 C 每读一次都要重新 attach 的共享内存。
    C 分得清的三种情形照旧分开：`ex_menu_tm` 还是 0 的版块打印「该版块精华区未开放」并等一个键；加载器读不出来的
    菜单等同于 `get_menu_shm_readonly` 失败，时间戳留在原处，下一轮再试；`VIEW_EX_ARTICLE` 指的帖子不在，树原样
    留在屏上。精华区的分页器只写 `display_ex_article_key_handler` 那一条状态栏（`menu_proc.c:203-216`）：没有
    同主题切换、没有回复，出去只能靠 `Esc`/`←`。
27. **数据文件管理页**——`data_file_mgr`（[`data_file_mgr::data_file_mgr`](src/data_file_mgr.rs)），上位菜单的
    `@DATAMGR` 条目（`bbs_cmd.c:28`），也是唯一会改写站点根目录下文件的菜单动作。屏幕上列出八个编号，回答按两个
    显示宽度读进三字节的缓冲区，于是 `atoi` 读出来的就是全部选择：什么都没输入、输入 `0`、输入一个超出列表的编号，
    都从原路回去。文件先按 `<路径>.user` 打开，站点没有这份副本时才读安装时的那一份；下面的编辑器与预览分页器，
    就是写入路径在用的 [`editor::editor_display`](src/editor.rs) 与 [`screen::display_file`](src/screen.rs)。
    C 的两个边角照旧：没有预览过之后的 `(S)` 不算回答，而守护进程正在退出时结束的编辑一个字都不写。保存会写两个
    路径——先写 `.user` 那份，因为重装时留下的正是它——然后按 C 的原话说明要重新加载配置才会生效，这也确实是事实：
    再读这些文件的是 [`load_data_files`](src/state.rs)。
28. **两段个人资料文字**——`user_intro_edit` 与 `user_sign_edit`
    （[`user_info_update::user_intro_edit`](src/user_info_update.rs)），上位菜单工具箱里的 `@EDIT_INTRO`/`@EDIT_SIGN`
    这一对（`conf/menu.conf:155-156`、`menu_proc.c:402-420`），也是那里唯二会写数据库行的条目。各自读 `user_pubinfo`
    的一列——`introduction`，或者签名档屏幕一次一格问出来的 `sign_1`…`sign_3`——交给第 27 条那个同样的编辑器，
    然后反复问 `(S)保存, (C)取消 or (E)再编辑? [S]: ` 直到某个回答把它定下来：`(C)` 和超时的按键直接离开、一条 SQL
    都不发，`(E)` 回编辑器再走一遍，其余回答重画这个问题，`CR`/`(S)` 则把文字过一遍敏感词、渲染、数行数，再写回去。
    这两屏有两个上限和字面数字对不上：打码是按字节而不是按字符出 `*`，行数里也算上收尾换行留下的那行空显示行，
    所以过得了十行检查的最长存档是九行。写回时 C 转义拼接的地方这里用绑定参数，说明档末尾那行引用的是花名册自己的
    60 秒刷新间隔，装不进 4 KB 缓冲的文字就是 C 的 `-3`：记一条日志，然后丢掉。曾和这两个名字并列的第三个
    `M_FAVOR_SECTION` 其实从来不是一个动作：C 是在问它的处理函数收藏夹子菜单的每一行可不可见
    （`menu.c:1148-1150`、`menu_proc.c:196-201`），这件事 [`Session::filter_allows`](src/session.rs) 已经做了。
29. **电子小鸡**——`chicken_main`（[`chicken::chicken_main`](src/chicken.rs)），入口是上位菜单的
    `【 电子小鸡 】`（`conf/menu.conf:238`），进入的方式与 C 完全相同：`@RunMBEM` 配
    `C@mod:game/so/libgame.so#chicken_main`，`#` 后面就是入口名；它花的糖糖走同一个 `libgame.so` 的另一半
    [`money`](src/money.rs)。整个游戏就是 `var/chicken/<UID>` 里的每个用户一行：十四个数字加一个名字，写的时候不
    截断，读回来的年龄不在文件里、由出生的月与日现算。屏幕是 C 的屏幕：`age` 挑出来的图案、`生日` 那个月与日、旁边
    补到八位的 `糖糖` 余额、两行帮助，外加一次只答一个键的九个动作——吃饭、吃零食、清理鸡舍、猜拳、目前战绩、买饲料、
    买零食、吃大补丸、卖鸡——再加 `c` 改名字、`k` 杀鸡、`t` 休息与 `q` 退出。每个数字都按 C 的方式动，包括那些没人会选的：
    休息那一屏先把累坏了的小鸡盖掉画出来、然后照旧执行这个动作，`tiredstrong` 只对付得起五十糖糖的人清零，数据库
    拒收的那笔存款会让鸡卖不掉、留下 `无法存钱，放弃交易！`，两百零一克就死。死了是 `呜...小鸡挂了....`，
    `var/chicken/log` 里多一行，记录文件被 unlink，然后在要求杀鸡的那个动作中间孵出一颗新蛋——用的还是那个开头写着
    `宝宝` 的取名提示。
30. **两个空转循环各自的时钟**——顶级菜单底下那块 活跃版块 面板（[`bbs::bbs_center`](src/bbs.rs)），以及 `w`
    打开的 十大热门榜（[`bbs::show_top10_menu`](src/bbs.rs)）。两者的走法受 C 写了两次是同一条规则约束：任何按键——
    除了 `KEY_NULL`/`KEY_TIMEOUT`——都刷新 `BBS_last_access_tm`，而一次超时只有在时钟已经走过
    `BBS_max_user_idle_time` 秒之后才离开（[`net::IDLE_LIMIT`](src/net.rs)、`include/bbs.h:36`），在那之前只是重画
    （`bbs_main.c:169-206`、`menu_proc.c:295-330`）。面板还多一个条件：它只属于顶级菜单，而且距上一次按键十秒之后
    才挪（`bbs_main.c:179`），所以子菜单打开时它是停下来，而不是在子菜单自己的画面底下重画一遍。
31. **累计访问人次**——欢迎页引用的那个访问总数，C 是在取在线列表的那一趟里读的，并没有自己的日程
    （[`user_list.c:370`](../lbbs/src/user_list.c)，从 `:620-661` 走过去）。重载先查在线列表，再
    `SELECT ID FROM user_login_log ORDER BY ID DESC LIMIT 1`，两个数一起发布出去
    （[`loader::Loader::reload_online`](src/loader.rs)）；`:394-401` 两个分支都会赋值，所以日志表是空的就把
    访问总数发布成 `0`，而不是留着上一趟量到的数。
32. **命令行那几样各打印什么**——`bbsd -h` 报的是 `arg_usage()` 报的字面 `bbsd`
    （[`daemon::PROG_NAME`](src/daemon.rs)），不是 shell 敲进来那条路径；`bbsd -C` 每个方括号里的答案都落在第 24 列，
    与 C 的制表位一致（[`daemon::flag_line`](src/daemon.rs)）；`@RELOADCONF` 在屏幕上回一句 `已发送指令` 再回菜单
    （[`bbs::Action::ReloadConf`](src/bbs.rs)、`menu_proc.c:150-180`），而不是向没人按的按键发问。
33. **加载路径写下的那几行**——池子装载完，报出来的就是 C 报的样子：花名册的
    `Rebuild index of %d users, last_uid=%d` 和 `Loaded %d users`（`user_list.c:243`/`:248`）、置顶链表和标记不一致时
    那两句 `Inconsistent state found: article %d …`（`section_list.c:1104`/`:1147`），以及一趟配置重载成功时写在
    common 流上的 `Reload section config successfully`（`section_list_loader.c:761`）——这几句都有测试把两条流抓下来
    逐字节比对。`Count of page exceed limit in section %d`（`section_list.c:1384`）是例外：它只在越过
    `PAGE_INDEX_CAPACITY`（[`paths.rs`](src/paths.rs)，按 `section_list.h:26-27` 每版 50 000 帖、每页 20 帖算出来是
    2 500 页）时才打印，所以这一句移植了却没被演练过。加载器自己的计数是格式化进消息本身的，
    没有当作 tracing 字段传，因为 C 的一行日志就是一个字符串——见
    [`log::format_line`](src/log.rs) 和钉住这一点的测试。
34. **两个跟着配置走的限制**——accept 那道关卡拿当前活着的会话数与重载刚发布的
    `bbs_max_client` 比较（[`net::Gateway::reserve_total`](src/net.rs)），这正是 `net_server.c:961` 每次 accept 都做一遍的
    `SYS_child_process_count - 1 < BBS_max_client`；因为地址被拒的那条连接会把总名额交回去，不会每拒一次漏一个。
    回复的标题缓冲也按 C 的数组来（[`post::TITLE_REPLY_BUF_LEN`](src/post.rs)）：
    `BBS_article_title_max_len + sizeof("Re: ")` 把结尾那个 `'\0'` 多算了一遍（`article_post.c:761`），所以是 165 字节，
    不是新发帖的 161（`:44`）。

### 6.2 在真实论坛数据库上验证过

一个 telnet 游客会话回答两个 5 秒提示、按 C 的文件缓存换行方式一屏读完 `data/welcome.txt`、以 `guest` 登录、走
`B` → 版块 → 帖子列表、用分页器读出 `var/articles/<CID>` 里的一篇真实帖子，然后被 `^P` 拒绝。
同一个列表再让游客按一下精华键 `m`：权限拦住了它，回来的只有那一片重画的行。
同样的会话从 russh SSH 进来：密码认证、欢迎页、分页器都一致。`SIGHUP` 记 `Reload configuration`；
`SIGTERM` 在收尾窗口内记 `Main process exiting` 与 `Main process exit normally`；每个会话都删掉了自己的
`user_online` 行——这几轮测试没有在那张表里留下任何东西。验证过程没有向生产数据库写入任何文章、短信或标记。

精华这条 SQL 阶梯只有真实库能源出证据，于是
[`the_digest_ladder_rolls_everything_back`](src/post.rs) 在上面跑一遍，
事务只 drop 不提交：标记与作者的经验值在事务还开着的时候读回来一次，回滚之后再读一次。回滚掉的 `INSERT`
仍然会让 `bbs_article_op` 的自增计数往前走一格，这是全过程留下的唯一痕迹。

在 `var/articles/` 为空的情况下启动，加载器把 C 那边目录树里的缓存全部重新渲染了一遍——28 220 个文件，
大约两秒——每个字节都与 C 为同一帖写出的文件一致，包括 `strftime` 那行表头，这也是服务器在启动时要先安装
`en_US.UTF-8` locale 的原因。C 的目录树里唯一少的那个文件，是版面上最新的一帖。

空闲的守护进程向数据库要了什么，不用写任何东西也量得出来：没人连的时候，`SHOW GLOBAL STATUS LIKE
'Com_select'` 每 15 秒涨 34——每秒一次 AID 扫描、一次操作日志重放，每五秒一次在线名单，每分钟一次花名册。
换成自己新建 loader 的那种周期循环，重放一次也不会发生，数字是 19；这两种形态的差别全在这个缺口里，
所以周期任务接的是初始加载留下的那个 loader。

三个生成工具也是这么核对的：每个 PHP 脚本都从一份沙盒副本对着真实库跑一遍——只读，`gen_ex_list.php` 的那条
`UPDATE` 换成 `SELECT 1`，好让版块的标记一个都不被清掉——再把 Rust 写出的文件逐字节比对。
`var/menu_merged.conf` 在 36 966 字节上完全一致，分类块、版块行、收藏行、精华行都填上了真实数据；
两个最大版块的精华区树分别以 90 087 与 55 959 字节一致；十大热门的两个文件则空、满两种形态都对上了——
线上版块最近七天没有帖子，所以先是 102 与 335 字节的两个表头，之后把沙盒 PHP 和 `gen_top` 那条
`#[ignore]` 测试里的日期窗口同样放宽到十年，得到 2 544 与 3 316 字节的满版本。

### 6.3 ⬜ 尚未完成

1. **Phase 2 的收尾**：GBK 的边角情况、SSH 的窗口大小（有意忽略，理由见 `src/ssh.rs` 文件头），
   以及与 C 就 Phase 1 全部界面做逐屏对照。

有两条从这份清单上划掉，而它们都不是还等着做的活。说明档/签名档编辑器是上面第 28 条。原先和它们并列
写着的“统计页”只是 `src/user_stat.c` 的一个叫法，而它根本不是一屏界面、也没有菜单键：它是资料页那个 发文 数字背后
的内存发帖计数器，已经作为 [`roster::UserStatMap`](src/roster.rs) 移植、由
[`userinfo::user_info_display`](src/userinfo.rs) 读取。`M_FAVOR_SECTION` 走的是同一条路——菜单逐行问的过滤条件，
早就有答案，从来不是一个只会记日志的键。于是第 13 节那行“再下一步”到此收尾：`bbs_cmd.c:19-40` 绑定的二十个名字里，
属于动作的十九个都有了处理函数。

所以目前仓库的状态是“**服务器能跑，游客可以浏览版块、读帖子，写入路径已接好并有单元测试**。
第一次真的发帖请先在测试数据库上做。

---

## 7. 搭建开发环境

### 7.1 需要的东西

| 项目 | 版本 | 说明 |
| --- | --- | --- |
| Rust | 1.88 或更新 | `Cargo.toml` 写的是 `edition = "2024"`，这要 1.85；另有四处 `if let … && let` 链式条件——[`net.rs`](src/net.rs) 与 [`term/`](src/term/key.rs)——用的是 1.88 才稳定下来的写法。`rust-version` 没有填，所以这两个下限都没人替你把关 |
| MySQL | 8.x | 存放论坛数据的那个库 |
| Linux | — | 开发与验证都在 Linux 上进行 |
| macOS | 11 或更新（arm64） | 已在 Apple Silicon 的 Mac 上原生构建并跑完整套测试，全绿；守护进程本身只验到 `Db::connect()`，再往下的启动步骤要有数据库。从 Linux 交叉时要自备两样 Mac 上本来就有的东西：一个认 Apple 参数的 clang——`aws-lc-sys` 与 `ring` 需要，zig 的就行——以及写明 `libiconv`（`libc` 自己声明的链接库）和 `CoreFoundation`（chrono 的时区后端）的 `.tbd` 桩，这两个通常来自 `MacOSX.sdk` |
| Windows | 10 或更新（x86_64，GNU 工具链） | 用 mingw-w64 能编过也能链接，测试在 wine 里跑通：库内 585 个、可执行文件里 6 个。比 unix 构建少的那 4 个是有意加 `#[cfg(unix)]` 的——`systemd` 那三个套接字地址的测试，还有文章缓存逐字节对比的那个，它的 C 基准文件把时区写成 `CST`，而 Rust 版在 Windows 上只能写 UTC 偏移 |
| locale | `en_US.UTF-8` | `locale -a` 里必须有它：服务器在写出任何文章缓存之前会安装这个 locale，与 C 一样，缺了就拒绝启动 |

`cargo` 是 Rust 自带的构建与测试工具。以下命令都在 `lbbs-rust/` 目录里执行。

```bash
cd lbbs-rust
cargo build            # 编译库和可执行文件
cargo test             # 589 个不需要数据库的测试（另有 14 个见 7.2）
cargo fmt              # 全树已经是 rustfmt 干净的，提交前跑一次
```

正常情况长这样：

```
test result: ok. 589 passed; 0 failed; 14 ignored; 0 measured; 0 filtered out
...
test result: ok. 6 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out
```

> **排版交给 `cargo fmt`，不用手工调。** 全树现在都能通过 `cargo fmt --check`，所以提交前先跑一次，
> 这样 diff 里就只剩下你真正改的东西。缩进仍是 4 个空格，下面的命名规矩不变。

GitHub Actions 把同样的两步搬到每台平台上：**CI** 先查格式，再 `--all-targets` 编译并跑测试，覆盖 Linux x86_64、
Linux aarch64 和 Windows x86_64 三趟，三趟都跑在 Linux 镜像上——Windows 那一趟用 mingw-w64 交叉编译，测试交给
wine——推送到 `main` 或 `v*.*` 分支、以及提 PR 时都会跑。打上 `v*.*.*` 标签则用 release 模式编同样的三趟，再加
macOS arm64：Linux 上交叉编出来的 Mach-O 在这里既跑不了也测不了，所以 Darwin 是唯一需要自己那台 runner 的平台，
而它只在出 release 时验一次，不在每次推送上占时间。wine 跑的是代码，不是平台，只属于真实 Windows 宿主机的行为——
比如解析器怎么对待一个连不上的数据库——只有真机才看得出来。同一个打标签的作业还会解开源码包照样编译测试一遍，
每个平台的产物都摆成一颗服务器根目录：`bin/` 里放四个可执行文件，再加 `conf/`、`data/`，以及守护进程不会自己建的
空 `log/` 和 `var/`，最后开一个草稿 release，附上压缩包和各自的 `.sha256`。作业需要的只有这份树本身：不用数据库
（那 14 个测试照旧 `#[ignore]`，见 §7.2），也不用预先生成 `var/`（那些由 §7.4 产出）。Windows 那一趟要用的
mingw-w64 gcc、`aws-lc-sys` 汇编要用的 NASM 和跑测试的 wine，都从镜像自己的包管理器装；跑测试的那步则指定缓存
基准文件剪出来时用的时区，因为它里面的 `strftime("%c %Z")` 跟着宿主机走。

### 7.2 使用真实数据库、真实配置文件的测试

有 14 个测试标了 `#[ignore]`，因为它们会连真实数据库，或者需要真实生成的配置文件。这些配置文件**不在 git 里**：
`var/menu_merged.conf`、`var/bbs_top_menu.conf` 和 `conf/bbsd.conf` 全被 `.gitignore` 挡住了——前两个由 §7.4 生成，
第三个装着密码。新克隆的树得先跑一遍 §7.4，再把 `conf/bbsd.conf.default` 复制过去、填上 `db_password`；只有早就
这么做过一遍的树，才谈得上“只要把位置告诉它们”。`LBBS_REF_CONF` 会决定会话测试从哪个根目录读 `var/`，所以要写绝对路径：

```bash
export LBBS_REF_CONF=$PWD/conf/bbsd.conf     # 先给它加上 db_password，见第 8 节
export LBBS_REF_MENU=$PWD/var/menu_merged.conf
export LBBS_REF_TOP10=$PWD/var/bbs_top_menu.conf
cargo test --all-targets -- --include-ignored
```

把这三个变量指向 C 目录（`../lbbs/...`）同样有效，而且更严格——那才是 C 亲手生成的文件。

如果想保持只读——对着线上正在使用的数据库时就该这样——把两条会写数据的测试排除掉：

```bash
cargo test --lib -- --ignored --skip live_flush
```

两条生成工具的对照测试正属于这个只读集合：`gen_top` 与 `gen_ex_list` 的各跑一条 `SELECT`，把输出写到 `/tmp`
底下，参照 PHP 自己的文件在场时逐字节比对（见 7.4）。`gen_ex_list` 绝不执行那条会清掉版块 `ex_menu_update`
标记的 `UPDATE`，因为那是一次写。

这几条是移植质量的核心关卡：验证 Rust 对 **C 真正生成的文件**的解读是否与 C 一致。
其中四条更进一步，在真实的帖子池上驱动一整场会话——每个版块的分页排布和每一行文字，
两个各自打开一个真实版块的游客（其中一个还会从 `var/articles/<CID>` 读一篇真实帖子再离开），
以及用加载好的 `user` 与 `user_online` 两张表画出的两张花名册页面：翻到末页，再用 `Esc` 退出。

> **注意**：`--ignored` **会连接真实的论坛数据库**。四条会话测试只读：在线心跳是
> `UPDATE … WHERE SID = ?`，它们用的是合成的会话号，匹配不到任何一行；游客也没有阅读记录或
> 阅读位置需要刷新。看版日志和收藏的测试**会写入**，跑之前请先备份。
> 你通过 `LBBS_REF_CONF` 传进来的文件里有密码——不要打印，也不要提交。

### 7.3 启动服务器

程序要求存在一个**服务器根目录**——放着 `conf/`、`data/`、`var/`、并且 `log/` 可写的那个目录——而它是从可执行
文件的位置往上两级算出来的，和 C 用 `bin/bbsd` 时的算法一样。本仓库本身就是这样的根目录，所以开发时要把编译
产物放进 `bin/`：

```bash
cargo build --release
mkdir -p bin && cp target/release/bbsd bin/
./bin/bbsd --foreground          # 读取 conf/bbsd.conf，监听 telnet 与 SSH
telnet -c localhost 2323            # 用 telnet 连接
ssh -p 2322 guest@localhost         # 用 SSH 连接
```

Cargo 包叫 `lbbs`，编译出来的守护进程叫 `bbsd`，和 C 服务的名字一样；7.4 里那三个生成工具分别叫
`gen_top`、`gen_section_menu`、`gen_ex_list`。`cargo run` 默认跑的就是守护进程。

只靠 `cargo run` 还是没法真的对外服务：二进制待在 `target/debug/` 下面时，算出来的根目录是 `target/`，那里
没有 `conf/` 可读。`--foreground`（C 的 `-f`）让服务留在前台；不加这个参数进程会脱离终端，只剩日志文件。

`--display-log`、`--display-error-log` 会把对应的日志同时打到 stderr；`--help`、`--version`、
`--compile-config` 名实相符。`SIGHUP` 重载配置，`SIGUSR1` 重开日志文件（配合 logrotate），
`SIGTERM` 会在退出前收尾在线会话：退出标记一置位，所有按键等待就立刻结束，停在提示上的会话因此
能走完自己的告别流程，而不是干等到空闲超时——C 的子进程就是靠父进程 `kill(0, SIGTERM)` 打断
`select()` 做到这一点的。

systemd 那边不需要 libsystemd：`Type=notify` 要等的三个状态由进程直接写到 `NOTIFY_SOCKET`——两个监听套接字
绑定完就发 `READY=1`，重载配置前后发 `RELOADING=1` 和 `READY=1`，退出标记一置位就发 `STOPPING=1`。所以 C 装
好的那份 unit——也就是 7.5 给这棵树渲染出来的同一份——也能用来起本程序。

开发树里那份 `conf/bbsd.conf` 监听 telnet `2323`、SSH `2322`（`bbs_port` 和 `bbs_ssh_port`），参考的那份 C 安装
用的是同一对端口——它那配置文件同样没有入库。入库的模板 `conf/bbsd.conf.default` 写的还是 C 的 `23` 和 `22`，
而 1024 以下的端口要 root 才绑得上。
用 telnet 连接时请加 `-c`：服务器会要求客户端停止本地回显，加 `-c` 可以避免协商完成前字符被显示两次。

> **同一个数据库同时只跑一个前端。** 本仓库的 `bin/bbsd` 和 C 版的 `bbsd` 写的是同样的
> `user_online`、`visit_log` 行；一起跑不会损坏数据，但在线列表和计数会重复。

游客会话能一路走完：两个提示、欢迎页、登录、上位菜单、讨论区组、版块、帖子列表，以及一篇真实帖子。
本地的 `tools/guest_smoke.py` 驱动的就是这条路径，它停在权限门口，因此不会写任何东西；对着上面启动的服务跑
`python3 tools/guest_smoke.py 2323` 即可。它只是手头的脚本，不是仓库内容——`tools/` 被 gitignore 挡住（见第 8 节）——
所以同样的走法你也可以在 `telnet -c` 里手工做一遍，或写一个自己的驱动。

### 7.4 运行批量生成工具

网页管理员以前用 crontab 跑的三个脚本现在是三个可执行文件。它们和 `bin/bbsd` 一样，从可执行文件往上两级
找出服务器根目录，所以要放在它旁边：

```bash
cargo build --release
ln -s ../target/release/gen_top bin/gen_top                     # 与 bin/bbsd 同样的做法
ln -s ../target/release/gen_section_menu bin/gen_section_menu
ln -s ../target/release/gen_ex_list bin/gen_ex_list
./bin/gen_top                    # var/bbs_top.txt、var/bbs_top_menu.conf
./bin/gen_section_menu           # var/menu_merged.conf：conf/menu.conf 加上数据库里的内容
./bin/gen_ex_list                # 每个被守护进程标了记的版块，各写一个 var/gen_ex/<SID>
```

三个都不接受参数：多一个参数就打印 `Invalid usage` 并以 `255` 退出，这正是它们在 `php` 下面做的事。
生成工具停下来的理由一律写到 **stdout**，因为 cron 任务寄出的邮件里装的是这条流，而不是那两个日志文件。退出
状态就是 PHP 自己的那几个数，透过 shell 的单字节看出来：致命的一步是 `255`（`-1`），查询失败或 `conf/menu.conf`
读不到是 `254`（`-2`），再外面一层的失败——版块列表，或者给 `ex_menu_tm` 盖章、顺手清掉版块
`ex_menu_update` 标记的那条 `UPDATE`——是 `253`（`-3`）。`gen_ex_list` 往 `var/gen_ex/` 里写，和守护进程自己
的那份一样，假定这个目录已经存在。

三次读数据库都是普通的 `SELECT`；生成工具里唯一的一处写，就是每个重生的版块那一条 `UPDATE`。对着线上
正在用的数据库，`gen_top` 与 `gen_section_menu` 可以随便跑，`gen_ex_list` 则留给真正持有那个标记的机器——
它的对照改由标了 `#[ignore]` 的 `a_real_digest_tree_is_the_php_bytes` 负责：只生成精华区树，什么都不清。

把生成工具和它替代的那段 PHP 比一比是很机械的：跑一份脚本的沙盒副本——其中的 `UPDATE` 换成 `SELECT 1`，
这样没有标记会被清掉——再对同一个数据库跑可执行文件，然后 `cmp` 两边的文件。6.2 里那些字节数就是这么量出来的。

### 7.5 安装

`make install` 把编译好的服务器摆成一颗服务器根目录，并渲染那两个服务配置文件——这就是 C 目录里 `make install`
做的事，布局、文件名和目的地都一样：

```bash
sudo make install                                    # PREFIX=/usr/local/lbbs，unit 装到 /etc/systemd/system
sudo make install PREFIX=/opt/lbbs                   # 任意绝对目录
make install PREFIX=$HOME/lbbs SYSTEMD_UNIT_DIR= LOGROTATE_DIR=   # 不要那两个服务文件
sudo make install DESTDIR=/tmp/stage                 # 全部落到暂存根下，unit 里仍写 PREFIX
sudo make uninstall PREFIX=/opt/lbbs
```

这些变量就是 [utils/install.sh](utils/install.sh) 的选项，不经过 make 也能直接跑：`PREFIX` 是服务器根目录，
`DESTDIR` 给每条路径加一个暂存前缀而 unit 里仍写真实前缀，`BBS_USER`/`BBS_GROUP`（默认 `bbs`）填 unit 的
`User=`/`Group=` 和 logrotate 的 `create`，`SYSTEMD_UNIT_DIR`/`LOGROTATE_DIR` 是两个服务文件的去处。哪个变量置空
就跳过哪个，Linux 以外两者默认就是空的。

装进去的是 `bin/` 里四个可执行文件、`conf/` 里三个 `*.default` 模板和 `menu.conf`、十一个 `data/*.txt`，以及空的
`log/` 和 `var/{articles,chicken,gen_ex}`——守护进程和 `gen_ex_list` 假定已经存在的目录；`conf/lbbs.service` 与
`conf/lbbs.logrotate` 分别由 [conf/lbbs.service.in](conf/lbbs.service.in) 和
[conf/lbbs.logrotate.in](conf/lbbs.logrotate.in) 按上面的前缀和账号渲染出来。不搬的是 `.gitignore` 点名的一切——
含凭据的 `conf/bbsd.conf` 等现用文件、`conf/ca_cert.pem`、SSH 主机密钥——它们属于这台机器，迁移安装时自己复制。
`make uninstall` 只删 `make install` 写下的那些文件，`log/`、`var/` 和现用配置原样留着。

Linux 上 unit 落到 `/etc/systemd/system/lbbs.service`，logrotate 片段落到 `/etc/logrotate.d/lbbs`。unit 就是 C 那份
原样——`Type=notify`、`ExecStart=<prefix>/bin/bbsd -f`、`KillSignal=SIGTERM`、`ExecReload=/bin/kill -HUP $MAINPID`
——因为移植版自己往 `NOTIFY_SOCKET` 写 `READY=1`、`RELOADING=1`、`STOPPING=1`（见 7.3），而片段里的
`systemctl reload` 恰好走到重启两个日志文件的 SIGHUP 路径上。`make install` 不建账号、不改属主、也不写
`conf/bbsd.conf`：这三步和 C 的 INSTALL.md 一样留给读者，脚本跑完会把它们打出来。release 压缩包本身已经是这个
形状的一棵安装树，解到 `/` 即可，不必再跑 install。

---

## 8. 配置文件与数据文件的位置

本仓库自带一个服务器根目录，因此不需要手边有 C 目录，就能替代 `bbsd` 或与之并行运行。git 里装的是人写出来的
东西：`conf/menu.conf`、三个 `*.default` 模板，以及十一个 `data/*.txt`。树里其余的一切都是生成出来的或只属于这台
机器，`.gitignore` 把它们全挡在外面——带密码的 `conf/bbsd.conf`、`conf/bbsnet.conf`、`conf/badwords.conf`、
`conf/ca_cert.pem`、主机密钥、渲染出来的 `conf/lbbs.service` 和 `conf/lbbs.logrotate`、整个 `var/`、`log/`，
还有 `bin/` 里那几个符号链接。所以刚克隆下来的树还不足以直接
跑：先按 §7.4 生成那些菜单，再复制 `conf/bbsd.conf.default` 补上凭据。

```
lbbs-rust/                      ← 服务器根目录（见 7.3）
├── Makefile                   ← `make install` / `make uninstall`，与 C 目录的同名目标（见 7.5）
├── utils/install.sh           ← 这两个目标实际跑的东西：目录、复制、渲染两个服务文件
├── bin/                       ← 指向编译好的 `bbsd` 与三个生成工具的符号链接；不进 git
├── conf/
│   ├── bbsd.conf.default      ← 主配置模板
│   ├── bbsd.conf              ← 你自己那份，含凭据；不进 git，见下文
│   ├── bbsnet.conf.default    ← 本站认识的其他站点（你那份 `bbsnet.conf` 不进 git）
│   ├── badwords.conf.default  ← 违禁词（你那份 `badwords.conf` 不进 git）
│   ├── menu.conf              ← 菜单定义，是 var/menu_merged.conf 的来源
│   ├── lbbs.service.in        ← unit 模板；安装时由它渲染出 lbbs.service
│   ├── lbbs.logrotate.in      ← logrotate 模板，同样方式渲染
│   ├── ca_cert.pem            ← MySQL 的 TLS 校验，只有文件存在时才用；不进 git
│   └── ssh_host_*             ← SSH 主机密钥：自己生成，绝不提交，见下文
├── data/                      ← 界面要显示的十一个文本：welcome、goodbye、eula、register、
│   │                             login_error、license、copyright、version、active_board、read_help、
│   │                             editor_help
│   └── *.txt.user             ← 用户改过的那一份，写在随站文本旁边，读的时候优先
├── var/                       ← 生成出来的、或运行时写下的；一样都不进 git
│   ├── menu_merged.conf       ← 守护进程真正读取的菜单集，由 conf/menu.conf 生成（§7.4）
│   ├── bbs_top.txt            ← 登录成功后立刻显示的画面，也是启动时预载的第十二个文本
│   ├── bbs_top_menu.conf      ← 同一次运行生成的十大热门菜单
│   ├── gen_ex/<SID>           ← 单个版块的精华区树，由 gen_ex_list 重写
│   ├── chicken/<UID>          ← 一个用户的电子小鸡记录，旁边还有一份 chicken/log；这个目录启动时创建
│   ├── articles/<CID>         ← 帖子正文——这是数据不是配置，见下文
│   ├── section_aid_loc/<UID>  ← “最后读到哪”的状态，随浏览写入
│   └── known_hosts            ← 本站往外穿梭时，对面那台机器的密钥，第一次连上时记下
└── log/                       ← bbsd.log 与 error.log
```

其中三处需要说明：

- **`conf/bbsd.conf` 是唯一需要你写的文件。** `cp conf/bbsd.conf.default conf/bbsd.conf`，然后把模板里的示例
  `db_*` 换掉（`user` / `db` 不是真实账号），以及模板里仍写作 23 和 22 的两个端口。`.gitignore` 里专门列了这个
  文件，免得密码一不小心被提交；模板本身不含任何可用凭据。没有密码时服务器根本不会启动：日志写下
  `Db::connect() error: ... Access denied for user '<who>'@'<host>' (using password: NO)` 然后退出。
- **`conf/ssh_host_{rsa,ed25519,ecdsa}_key` 由你自己生成。** SSH 监听会读这三个密钥，一个都读不到就放弃启动
  （`ssh::load_host_keys`），但把线上环境的签名密钥搬进开发目录正是最不该做的事：

  ```bash
  for k in rsa ed25519 ecdsa; do ssh-keygen -t $k -f conf/ssh_host_${k}_key -N '' -q; done
  ```
- **`var/articles/` 是内容，由服务器自己生成。** 索引在 `bbs` 表，正文在 `bbs_content` 表，读者看到的那个文件
  是把两张表渲染进 `var/articles/<CID>` 得来的；文件不存在就是硬错误，日志写作
  `article_cache_load(aid=…, cid=…) error`，与 C 一致。
  加载器在收录新帖时就把这些文件写出来（[`cache::generate`](src/cache.rs)
  就是那套渲染逻辑的移植，且与 C 逐字节一致），所以一个空目录会自己从数据库里填满——测试论坛的 28000 个文件、
  130 MB 大约两秒——不必再从别的目录复制。它照样不进 git，`.gitignore` 已经替你做了这件事。

Rust 里这些路径集中放在 [src/paths.rs](src/paths.rs)，
再相对于 [`Session`](src/session.rs) 持有的服务器根目录解析。

`conf/bbsd.conf` 的键名（与 C 完全一致，含义一致、怪癖也一致）：

| 文件里的键 | Rust 字段 | 含义 |
| --- | --- | --- |
| `bbs_id` | `bbs_id` | 论坛内部名称（例如 `LBBS-devel`） |
| `bbs_name` | `bbs_name` | 界面显示名（例如 MyBBS） |
| `bbs_server` | `bbs_server` | 域名 / 对外公布的地址 |
| `bbs_address` | `bbs_address` | 监听地址 |
| `bbs_port` | `telnet_port` | telnet 端口（模板里是 23，本树那份本地配置是 2323） |
| `bbs_ssh_port` | `ssh_port` | SSH 端口（模板里是 22，本地是 2322） |
| `bbs_max_client` | `max_client` | 全局并发连接上限 |
| `bbs_max_client_per_ip` | `max_client_per_ip` | 单 IP 并发连接上限 |
| `bbs_start_dt` | `start_dt` | 开站日期，写作 `YYYY-M-D` |
| `bbs_sys_id` | `sys_id` | 系统 ID |
| `db_host` / `db_username` / `db_password` / `db_database` / `db_timezone` | `db` 子结构 | MySQL 连接信息 |

解析细节也照搬了 C，而“改了配置没生效”通常就出在这里：

- 分隔符是空格、制表符、CR、LF，因此值里不能有空格（文件开头的注释也是这么写的）。
- **键名不区分大小写**（C 用的是 `strcasecmp`）。
- 第一个词以 `#` 开头的行按注释跳过。
- 只有键、没有值的行会警告 “Skip empty value” 并跳过。
- 有**第三个词且它不是以 `#` 开头**的行，**整行会被丢弃**。
  例：`bbs_port 2323 2324` → 警告并跳过，值不生效，**端口仍是默认值 23**。
  `bbs_port 2323 # 备注` → 生效。
- 数字按 C 的 `atoi` 规则读：遇到第一个非数字就截断，所以 `bbs_port 2323a` 会读成 2323。
- 超出范围的值（如 `bbs_max_client 0`）会警告并**不应用**。
- 字符串值按 C 的缓冲区长度截断（`bbs_id` 20、`bbs_name` 50、`bbs_server` 255）。

### ⚠️ 关于安全，必须知道的事

- `db_password` 在配置文件里是**明文**存的。这是从 C 版继承下来的做法。
  **请不要把这个文件提交进 git；任何时候都不要把它的值输出**到日志、错误信息或本仓库里。
  移植过程中的验证也只通过程序传值，从不打印。`.gitignore` 就是这条承诺的落实手段：`conf/bbsd.conf` 被忽略，
  进仓库的是 `conf/bbsd.conf.default`，本仓库的任何一棵目录树里都不曾出现过那个值。
- 只有 `conf/ca_cert.pem` 存在时，MySQL 连接才**校验服务器证书**（[`db::options`](src/db.rs)）；没有这个文件时
  sqlx 退回 `PREFERRED`，这正对应 C 客户端的 `SSL_MODE_PREFERRED`。这个文件不进 git，因为它是某台主机的 CA，
  不是移植产物。
- 原 C 版在登录失败时，会把**明文密码**写进 `user_err_login_log`。为了数据兼容（网页端读同一列），
  我们原样保留了这个行为。移植**并没有**让这张表变安全：请把它当作敏感数据，并且把它看作部署环境里一个真正的
  改进项，而不是在移植过程中顺手改掉的东西。
- 口令哈希是数据库侧计算的 `SHA2(明文, 256)`，没有加盐。为了保证网页端账号可用，未做改动。

---

## 9. 一次连接的完整过程

按 C 版真实顺序列出用户会看到的东西。最右一列给出 Rust 中的位置和状态。

| # | 用户看到什么 | C 中的处理 | Rust 中的位置 / 状态 |
| --- | --- | --- | --- |
| 1 | 建立连接 | `net_server.c` accept 后 fork | [`net::listen`](src/net.rs) 一条连接一个任务，连接数检查与 `Accept ... connection from` 日志都与 C 一致 ✅ |
| 1a | 走 SSH 进来 | `net_server.c` 的 libssh 子进程 | [`ssh.rs`](src/ssh.rs)：密码认证之后跑同一段会话；`欢迎使用ssh方式访问` 顶替登录提示 ✅ |
| 2 | 被问 `(U)UTF-8/(G)GBK? [U]` | `bbs_charset_select`，两次 5 秒超时提示 | [`bbs_charset_select`](src/bbs.rs) ✅ |
| 3 | `(V)变宽/(F)定宽`（宽度模式） | 同一组提示 | 同一次调用，两个答案都接到 [`Terminal::set_fixed_width`](src/term/mod.rs) ✅ |
| 4 | 欢迎画面 | 以 `display_file(..., 2)` 显示 `data/welcome.txt` | [`Session::display_file`](src/session.rs#L247) ✅ |
| 5 | “请输入帐号” | `bbs_login`，最多 3 次 | [`login()`](src/login.rs#L160) ✅ |
| 6 | 输入 `guest` 或 `new` | 试用账号 / 注册说明 | ✅（`new` 显示说明后结束会话） |
| 7 | “请输入密码” | `check_user` | [`check_user()`](src/login.rs#L235) ✅ |
| 8 | 失败次数过多被挡住 | /24 网段 10 分钟内 10 次；单账号 3 次 | ✅（`ipmask::subnet_pattern` + `db::count_errors_by_*`） |
| 9 | `您目前无权登陆...` | 原样复现，包括“先写日志再判断”的顺序 | ✅ |
| 10 | “用户协议已更新” | `user_update_agreement` | [`update_agreement()`](src/login.rs#L418) ✅，两种传输都会调用 |
| 11 | 上线登记、访问计数 | `user_online_add`、`visit_log` | [`Session::online_add`](src/session.rs#L192) ✅ |
| 12 | `var/bbs_top.txt` + 上位菜单 | `display_file(VAR_BBS_TOP)` → `bbs_center` | [`bbs_main`](src/bbs.rs) ✅，含十秒一次的 `data/active_board.txt` 面板 |
| 13 | 选版块 → 选帖子 → 阅读 | 菜单条目 + `display_file` + `section_list_*` | 菜单解析与跳转 ✅，[`browse`](src/browse.rs) ✅ —— 这正是 `tools/guest_smoke.py` 走的那条游客路径 |
| 14 | 发帖、回复、修改、删除 | 编辑器 + 写 `var/articles/` + 更新数据库 + 违禁词 | [`editor`](src/editor.rs) ✅，[`post`](src/post.rs) ✅（每个动作一个事务，提交时过滤违禁词）；游客在任何 SQL 之前就被拒绝 |
| 15 | 经验值结算与告别 | `user_online_exp` + `data/goodbye.txt` | [`Session::logout`](src/session.rs#L359) ✅ |
| 16 | 从在线列表消失 | `user_online_del` | [`Session::cleanup`](src/session.rs#L390) ✅ |

还是 ⬜ 的那些格子是这些按键背后的界面，见 6.3 节。

---

## 10. 改了的，和刻意没“修”的

### 10.1 有意改动（机制层面）

| C 的做法 | Rust 版 | 原因 |
| --- | --- | --- |
| 每个连接 `fork()` | Tokio 任务，单进程 | 安全性、资源占用，且 MySQL 连接数大幅下降 |
| 进程全局 `BBS_*` | `Session` / `AppState` 字段 | 多会话共用一个进程后，状态必须显式 |
| POSIX 共享内存 + 信号量 + `mprotect` | `Arc<RwLock<...>>` | 同上 |
| `mysql_real_escape_string` + `sprintf` 拼 SQL | sqlx **绑定参数** | 从根上消除 SQL 注入这一类问题 |
| 事务一直开着，靠 `mysql_close` 收尾 | 每条路径显式 `commit` / `rollback` | 连接池里带着未结束的事务还回去，会坑到下一个使用者 |
| `setenv("TZ", ...)` + `tzset()` | 记录在 `Session::user_tz` | 改进程环境会泄漏到其他会话 |
| 重载结束时 `kill(0, SIGUSR1)`，让子进程重读父进程刚加载的东西（`net_server.c:877`） | [`reload_configuration`](src/main.rs) 发布新的 `Arc` 快照，会话下一次画屏读到的就是它 | 已经没有进程可以发信号了。常驻表的刷新在 C 那边本来就是**定时器**：loader 按 `BBS_section_list_load_interval` 循环（`bbs.c:38`、`section_list_loader.c:843-845`，[`Loader::run`](src/loader.rs) 照此实现），而 `SIGUSR1` 只负责重开日志文件（`bbs_main.c:41-47`），父进程干脆把它忽略了（`main.c:443-447`） |
| 每个监听套接字同时设 `SO_REUSEADDR` **和** `SO_REUSEPORT`（`net_server.c:634-645`） | 只设 `SO_REUSEADDR`（[`net::bind`](src/net.rs)） | 后一个选项是让好几个进程绑同一个端口、由内核把连接分给他们；`fork()` 去掉之后监听的就一个，设上它只会把“另一个 `bbsd` 还在跑”从一次失败的 bind 变成两个守护进程悄悄共用一个端口。现在重启会响亮地失败，而不是并行着互相抢连接。 |
| 父进程在 `SYS_child_process_count` 里数自己的子进程，每次 accept 拿它和 `BBS_max_client` 比（`net_server.c:961`） | [`net::Gateway::reserve_total`](src/net.rs) 拿当前活着的会话数与重载刚发布的上限比 | 最早的移植在启动时就把信号量定死了，而那恰恰是 C 那个数永远不可能做到的事：把 `bbs_max_client` 调大调小，下一次的 accept 关卡就跟着动，和 C 一样。 |
| `arg_usage()` 打印的是字面量 `bbsd`（`main.c:83`） | 同样是字面量（[`daemon::PROG_NAME`](src/daemon.rs)） | `argv[0]` 是 shell 敲进来的那条路径，照它打印就会输出 C 从来不会打印的 usage；服务器根目录仍然由 `argv[0]` 推出来，与 `main.c:217` 一致。 |
| 一次读只收到半个转义序列时整段丢掉、这次读回答 `ESC`（`io.c:430-487`），于是那一下按键和紧跟在后面输入的键一起消失 | [`Terminal::read_key`](src/term/mod.rs#L336) 先等十分之一秒把剩下的字节接上、重试到序列完整；只有缓冲区里本就只剩一个孤立 `ESC` 时才回答 `ESC` | 真连接上箭头键本来就会被 TCP 切开，用户看到的是“Enter 或 Del 要按两次才生效” |
| `ESC [ H`、`ESC [ F` 不在 C 的两字节表里（`io.c:802-822`），于是被当成正文送进编辑器，画成一个星号 | [`key::decode`](src/term/key.rs) 把它们读成 Home 与 End | xterm 系的客户端发的就是这两个键的这串字节 |
| 一批按键共用一次重画，而最后那个键算出的行区间说了算：`Ctrl-Y` 先清空它下面的行，跟在后面的 `Ctrl-K` 把区间收窄成一行（`editor.c:947`、批量 `continue` 见 `:1093-1105`），`Ctrl-W`/`Ctrl-X`（`:1117-1120`）更是在刚敲的那一行画出来之前就返回 | [`View::cover_stale`](src/editor.rs) 把每次的区间重新放宽到这一批清空过的所有行，退出键也等这一趟画完 | 字节一直都在文件里，只是屏幕没跟上；重开这篇帖子它们又出现了 |
| `editor.c` 每次编辑只重新拆分改动的那条数据行，再挪动显示指针数组 | 同样只重排那一行（[`EditorData::recount_line`](src/editor.rs)） | 最早的移植每敲一个字符就把整个文档重排一遍，黏贴长帖时开销随长度平方增长、把一个 CPU 核跑满，而 C 几乎不耗 CPU |
| 每一局猜拳里的 `srand((unsigned) time(NULL))` 加 `rand() % 3`（`chicken.c:726-727`） | [`chicken::opponent`](src/chicken.rs) 混的是同一颗以秒为单位的种子 | glibc 的加法反馈发生器是 libc 的性质而不是这个游戏的性质，照搬过来等于多维护一个 libm。种子仍然一秒变一次、仍然只有三个答案，所以一秒之内猜两次碰到的还是同一只手——这才是玩家看得见的那部分。 |
| 小鸡记录用 `fscanf` 读：十四个 `%d` 加一个 `%s`（`chicken.c:117-118`） | [`Chicken::parse`](src/chicken.rs) 把这一行切开，逐格交给 [`c_atoi`](src/config.rs) | C 拿去和 `15` 比较的格数这里照样报得出来，C 读不到的那一格这里读成 `0`——凡是 `save_chicken` 与 `create_a_egg` 写过的行，两边结果一致 |
| 显示行数已经占满 [`MAX_EDITOR_DATA_LINES`](src/paths.rs) 之后再编辑，需要新行的那部分内容就地丢掉：被改的那一行截短，并在保留的末行末尾粘一个 `'\n'`（`:366-387`）；若改动在更前面，则把数组最后一行削到少一格（`:447-458`） | 同处在到达上限时 | 最早的移植改成拒绝这次编辑，于是一篇写满的文章再也输不进东西；C 在这里是静默截掉尾部。只有一种情形不复现：在一份占满数组的文档里删除最后一行，C 数出来的行数比文档实际多一行——那是它数组里一条不再维护的残留记录，画出来是空的，而移植画的是真实存在的那一行；两种算法存盘的字节一致。 |

### 10.2 原样复现的 C 语言“怪癖”

移植中最危险的事，就是“好心”把原来的 bug 修掉：显示和数据都会变，网页端就会和我们对不上。
因此下面这些行为是**逐字复现**并用测试钉住的。

| 原始行为 | 位置 | 为什么不能“顺手修好” |
| --- | --- | --- |
| `ip_mask` 会在**第一个被掩码的字节处截断字符串**，所以带第五段的地址尾部会消失 | [`ipmask.rs`](src/ipmask.rs) | 限流用的 `LIKE '202.38.64.%'` 正是依赖这个表达式 |
| 用户名/密码**格式校验阶段一条 SQL 都不发**，所以格式错误的尝试不会被记录 | [`login.rs`](src/login.rs) | 网页端的解锁功能依赖这些计数 |
| 账号被停用（`p_login = 0`）时，登录日志**先提交**再拒绝 | 同上 | 否则访问计数会变 |
| 走 EULA 流程时既不更新访问计数，也不登记在线 | 同上 | 同上 |
| `moveto` 遇到负数会带符号输出；行号为负时只输出一个 `\r` | [`Terminal::moveto_signed`](src/term/mod.rs#L292) | 菜单光标位置依赖这个行为 |
| `S_DEFAULT = 0x3`（可看列表 + 可得经验值），**不含发帖位** | [`privilege.rs`](src/privilege.rs) | 让新账号“更方便”就会与网页端不一致 |
| 被对方**拒绝**的连接不会立刻报出来，要等十秒预算走完，然后报 `连接超时！` | [`bbsnet::connect`](src/bbsnet.rs) | C 的等待循环在“可写但没连上”的套接字上继续空转，进度条期间按下的键也被连接吃掉；提前失败会同时改掉屏幕上的字和真正送到站点的按键 |
| 编辑器的**十行上限其实只容得下九行**：`split_data_lines` 是 do/while，收尾第十行的那个换行自己又算出一行显示行 | [`user_info_update.rs`](src/user_info_update.rs)、[`text::split_data_lines`](src/text.rs) | 按读者的数法数行，就会接受 C 用 `长度超过限制 (10行)` 拒掉的文字；而同一个计数器也决定资料页与版块列表一屏放得下什么 |
| 恰好删到一条数据行结尾的那次删除，会把**光标所在的那个显示行**留成一行零宽的显示行：`editor_data_delete` 只从改动的那一行起重新拆分这一条数据行，不是从行首重来（`editor.c:714-734`） | [`EditorData::delete`](src/editor.rs) | 从行首重排会丢掉这一行，光标所在的那一行于是跑到文档末尾之后，下一个按键被定位回第 0 行——结果就是往第一行上打字 |
| 取名的提示**开头本来就带着 `宝宝`**（`chicken.c:175`），而 `get_data` 把这段预填当作行首来计数（`screen.c:326`）：单按回车小鸡就叫 宝宝，敲进来的名字也接在它后面——先 `宝宝` 再敲 `新鸡`，收下的名字是 `宝宝新鸡` | [`chicken::create_a_egg`](src/chicken.rs) | 同一块缓冲也拿现名去预填 `改名字` 那一问，所以一个会清掉预填的编辑器会把小鸡改成主人从没输入过的名字 |
| `chictime` **只读、从不赋值**：`show_chicken`（`chicken.c:221-229`）在它超过 200 时让小鸡变瘦变脏，而 `chicken.c` 没有任何一行给它赋值，所以 C 描述的那个又饿又脏的鸡舍不会发生 | [`chicken::show_chicken`](src/chicken.rs) | 这是说明里有、代码里没实现的功能；替它补上实现就是改设计，字段留着只是为了记录仍然读得出同样那十四个数字 |

这也正是这张表的规矩：条目只能因为参考端先离开而离开，不能因为我们觉得 C 是错的。

真要对上述任何一条做修复，应当与数据、网页端一起作为一次协同变更，绝不能在移植里悄悄改掉。

---

## 11. 在本项目里写代码的规矩

- **缩进 4 个空格，其余排版交给 `cargo fmt`。** 全树要保持能通过 `cargo fmt --check`；手工排的版只会在下一次
  格式化时被推翻。
- 每个模块头部必须写明移植自哪个 C 文件的哪些行，并带一节 `# Deviations from C`。
  **两者缺一的移植会被退回。**
- C 的常量（`BBS_max_section = 200`、`USERNAME_MAX_LEN = 12` 等）统一放进 `paths.rs`，不要写裸数字。
- 不需要数据库的测试用 `#[test]` / `#[tokio::test]`；要连真实库的必须 `#[ignore]`，并靠 `LBBS_REF_*` 环境变量取路径。
- 离线测试需要一个 `Session`？用 [`session::offline_session()`](src/session.rs)：
  它构造的是延迟连接池，不会真的建立连接，也不发任何查询。
- 在 `async fn` 里到处传 `&mut Session` 时注意借用冲突。例如
  [`Session::display_file`](src/session.rs#L247) 是递归的——
  分页器可以请求显示帮助，而帮助本身又是显示文件——所以它的 future 用 `Pin<Box<dyn Future<...>>>` 装箱；
  普通的 `async fn` 无法拥有递归的返回类型。
- 新增 SQL 之前，先读 C 里同一件事的上下文，**尤其是何时 commit**。
- 输出先缓存在 `Terminal`，只有 `flush`（对应 C 的 `iflush`）才会真正发到线上。
  读取按键之前一定会先 flush，所以提示语不会被吞掉。

### 11.1 先记住这几个类型就够了

| 类型 | 文件 | 一句话 |
| --- | --- | --- |
| `AppState` | `state.rs` | 所有连接共享的东西：配置与菜单快照、数据库池、装载好的随站文本、违禁词 Trie，以及常驻的 `Board`、`Roster`、`OnlineList` |
| `Session` | `session.rs` | 一次连接：uid、用户名、权限、对端地址、当前动作、时间戳 |
| `Terminal` | `term/mod.rs` | 这条连接的带缓冲输入与输出 |
| `Config` / `DbConfig` | `config.rs` | 解析后的 `bbsd.conf` |
| `Db` | `db.rs` | sqlx 连接池；内部是 `Arc`，所以 `Clone` 很轻 |
| `UserPriv` | `privilege.rs` | 一个人的等级、全局权限位、各版块的权限位 |
| `MenuSet` / `MenuItem` / `Action` | `menu.rs` | 菜单条目，以及按下某个键要执行哪个功能 |
| `Board` | `board.rs` | 版块、只往右增长的帖子缓冲区，以及每个用户的发文计数——人本身在上一层的 `Roster` 和 `OnlineList` 里 |

---

## 12. 常见问题与容易踩的坑

**问：为什么要如此死磕 C 的细节？干净地重写一遍不是更好吗？**
答：因为数据库和文件是和网页端**同时**在用的。帖子显示差一列、正文截断方式不同、权限突然变大——这些都算移植失败。
原程序的行为，就是本项目最接近“规格说明书”的东西。

**问：界面为什么还是中文？**
答：文字来自 `data/*.txt` 以及 C 源码里的字符串常量。原程序没有本地化机制，网页端显示的也是同样的文本，
在移植时翻译就会和数据对不上。

**问：我的测试报 “this functionality requires a Tokio context”。**
答：`Db::detached(...)` 只能在运行时上下文中创建。把该测试改成 `#[tokio::test]`。

**问：全角字符对不齐。**
答：C 有“定宽 (F) / 变宽 (V)”两种选择，宽度算式不同。请取 [`Terminal::fixed_width`](src/term/mod.rs) 的答案——
这个开关挂在连接的终端上，不在 `Session` 上——再把它作为参数传给 `text::display_width`，不要用默认值。

**问：输入密码时屏幕上出现了明文。**
答：不应该出现。会话一开始就送出 `IAC WILL ECHO, IAC WILL SGA`
([`screen::set_input_echo`](src/screen.rs))，客户端据此不再自己回显；
`NOECHO` 提示接着每字符打印一个 `*`，回车时再擦掉。若明文照旧可见，那就是传输层在第一个提示之前
没把六个协商字节写出去——只有掩码挡不住客户端自己的回显。

**问：我想和本地已有的网页版一起试用。**
答：把 `conf/bbsd.conf` 指向**备份库或测试库**，`db_password` 只留在这个文件里（不提交、不打印）。

**问：帖子正文在哪？**
答：`var/articles/<CID>`。`CID` 就是 `bbs` 索引行上那一列（`UNIQUE KEY CID`），也是它对应的 `bbs_content` 正文
的主键——数据库里并没有 `article` 表。C 用 `fopen` 打开；移植版同样是直接打开文件
（[`cache::load`](src/cache.rs)，由
[`browse`](src/browse.rs) 调用），写则由加载器完成
（[`cache::generate`](src/cache.rs)，由
[`loader`](src/loader.rs) 调用）。联机读到一篇真实帖子的过程见 6.2 节。

**问：我的脚本在驱动会话，按键却没有反应。**
答：`display_data` 里 `Enter`/`↓` 只滚**一行**，整页要靠空格或 `PgDn`（`screen.c:772-814`），
而它不认识的键一个字节都不输出（`screen.c:837-839`）。所以“发一个键、睡几秒”的脚本会把一份四十多行的
欢迎页当成卡死。请改成等到期望的文字出现再发下一个键，翻页用空格。

---

## 13. 后续计划

| 步骤 | 工作内容 | 完成标准 |
| --- | --- | --- |
| 已完成 | 监听：裸 TCP + russh SSH、连接数限制、真正的 `main.rs`、EULA 接入 | `bin/bbsd --foreground` 在两个端口都能接受连接（见 7.3——`cargo run` 做不到，因为它推出来的根目录里没有 `conf/`） |
| 已完成 | 对论坛数据库做联机冒烟测试：游客登录、菜单、SSH 认证、重载、重开日志 | 同一批界面表现得和 C 一致 |
| 已完成 | 打通浏览功能：`list_section`、帖子列表、阅读、`LOCATE_ARTICLE` | 按菜单键能进版块，联机游客会话能读到一篇真实帖子 |
| 已完成 | 写入路径：新帖/回复/修改/删除、编辑器、索引更新、缓存生成、提交时过滤违禁词 | 有单元测试覆盖，且联机游客按 `^P` 在任何 SQL 之前被拒绝 |
| 已完成 | `utils/bin` 的三个 PHP 生成工具改成可执行文件：`gen_top`、`gen_section_menu`、`gen_ex_list`（见 7.4） | 对着真实库，它们写出的文件与 PHP 逐字节一致（见 6.2） |
| 下一步 | 在**测试库**上完整走一遍真实发帖，再依次验证回复/修改/删除 | 网页端能看到移植版写出的帖子，头信息与缓存文件和 C 一致 |
| 已完成 | 剩下的菜单条目：资料编辑那两个编辑器（见 6.1 第 28 条），以及弄清 `M_FAVOR_SECTION` 到底是什么——它是一个菜单过滤条件，早就移植过了 | 上位菜单里不再有“只写日志”的按键 |
| 已完成 | `@RunMBEM` 点名的 `libgame.so` 两个入口：电子小鸡，和它花的糖糖（见 6.1 第 29 条） | 小鸡那一屏、它的九个动作与它的死亡都对着一个记录文件跑过，除了糖糖事务之外不写数据库 |
| 已完成 | 第一轮漏掉的 C 行为：每次 accept 现读的那两个连接数限制、`-h`/`-C` 那条命令面、`@RELOADCONF`、加载路径上的几行日志、两个空转循环的时钟与面板轮转（见 6.1 第 30–34 条） | 每一条都有一个不需要数据库的测试，`bbsd -C` 的六个 configure 答案都落在第 24 列 |
| 最后 | 与 C 版就 Phase 1 的全部界面对比 | 同样操作得到同样的画面 |

---

## 14. 延伸阅读

- **原始 C 源码**：`../lbbs/src/` 与 `../lbbs/include/` —— 只读。
- [reference/schema.sql](reference/schema.sql) —— 完整数据库结构。**看不懂某张表时先看这里。**
- [reference/sql_inventory.md](reference/sql_inventory.md) —— C 版发出的所有 SQL 清单。**排查漏移植时先看这里。**
- [tests/fixtures/](tests/fixtures) —— 测试用帖子文件，以及从 C 版采集的基准（golden）输出。
- 每个模块开头的注释 —— 说明它移植了什么、与 C 差在哪。**这才是最详细的文档。**

## 15. 许可证

GPL-3.0-or-later，与原 LBBS 相同。
原作者：Leaflet &lt;leaflet@leafok.com&gt;（Copyright (C) 2004-2026）。
