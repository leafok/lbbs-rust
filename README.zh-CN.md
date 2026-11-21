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
| 版本 | 1.7.12 —— 即 `main` 分支上的 `b7a8b16`（这一版还没打 `v1.7.12` 标签） |
| 语言 | C（`src/*.c` 加 `include/*.h` 约 29,850 行） |
| 许可证 | GPL-3.0-or-later |
| 原作者 | Leaflet &lt;leaflet@leafok.com&gt; |
| 与本项目同级的参考副本 | `../lbbs` —— 只读；**我们不会修改它** |

LBBS 是一个**用纯文本终端访问的论坛（BBS）服务器**。用户通过 telnet 或 SSH 连上来，按菜单键看版块、发帖。
没有鼠标，也没有图片：画面完全由文字加上**终端控制序列**（形如 `ESC[...H` 的字符串，用来设置颜色和光标位置）画出来。

另一个关键点：**LBBS 并不自己保管数据。** 帖子正文和附件是磁盘上的文件，而用户、版块、帖子的索引导在一个
**MySQL 数据库**里。本站的**网页版论坛**读的是同一个数据库。所以 LBBS 实际上是 *那个网页版论坛的终端前端*，
这带来一条硬约束：**不能擅自改数据库结构。**

参考端的 1.7.12 是 `v1.7.11..b7a8b16`：十八个提交、19 个文件、+106/−40 行，工作区与 `b7a8b16` 一致。
Rust 源码里出现的每一个 `file.c:NNN` 引用都对准上面这个修订号，也就是说
`git -C ../lbbs show b7a8b16:src/<文件>` 打印出来的，正是那条注释所描述的内容。
[第 10.3 节](#103-1712-这一轮参考端的修改在-rust-版里对应什么)记录 1.7.12 的每个修复在这里落在何处。

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
| 版块 / section | 论坛的分类（比如“诗词”“电脑”）。数据库表是 `section`，其 ID 称为 **SID** |
| 帖子 / article | 版块中的一条投稿，其 ID 称为 **AID** |
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
2. **版块、帖子、用户清单常驻在共享内存里**，靠 `SIGUSR1` 等信号刷新，而且刷新是**增量的**：只重读发生变化的行。

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
| Phase 2 | 精华区树、统计、用户资料编辑器、GBK 完善、SSH 窗口尺寸 | 已开始一部分：精华区树可以浏览，其余未动 |

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
| [src/paths.rs](src/paths.rs) | 116 | `include/bbs.h` 等 | 屏幕尺寸、各字段长度、路径名等常量 | 完成 |
| [src/config.rs](src/config.rs) | 384 | `src/init.c` | 读取 `conf/bbsd.conf` | 完成 |
| [src/text.rs](src/text.rs) | 357 | `src/str_process.c` | 显示宽度（全角=2 列）与折行计算 | 完成 |
| [src/term/mod.rs](src/term/mod.rs) | 697 | `src/io.c` | 字节管道、GBK 转码、`prints`/`iflush`、`igetch` | 完成 |
| [src/term/key.rs](src/term/key.rs) | 504 | `src/io.c` | 把 ESC 序列翻译成按键码 | 完成 |
| [src/screen.rs](src/screen.rs) | 1116 | `src/screen.c` | 清屏、回显接管 `set_input_echo`、行输入 `str_input`、分页器 `display_data` | 完成 |
| [src/db.rs](src/db.rs) | 1368 | `src/database.c` 等 | 经 sqlx 访问 MySQL | 完成 |
| [src/board.rs](src/board.rs) | 1474 | `src/section_list_loader.c` | 版块/帖子/用户的内存池 | 完成 |
| [src/browse.rs](src/browse.rs) | 3290 | `src/section_list_display.c`、`src/section_list_loader.c`、`src/menu_proc.c`（两个精华区入口） | 帖子列表、列表按键、阅读帖子、阅读位置文件，以及版块精华区树的浏览与树里读一篇帖子 | 完成 |
| [src/editor.rs](src/editor.rs) | 1812 | `src/editor.c` | 全屏编辑器：行列、窗口、状态行、按键 | 完成 |
| [src/post.rs](src/post.rs) | 1488 | `src/article_post.c`、`src/article_del.c`、`src/article_op.c` | 发帖、回复、修改、删除、精华标记，以及各自的事务 | 完成 |
| [src/viewlog.rs](src/viewlog.rs) | 366 | `src/article_view_log.c` | 记录该用户读过哪些帖子 | 完成 |
| [src/favorlog.rs](src/favorlog.rs) | 446 | `src/article_favor.c` | 记录该用户收藏了哪些帖子 | 完成 |
| [src/favorlist.rs](src/favorlist.rs) | 768 | `src/article_favor_display.c`、`src/article_favor.c`（取页）、`src/menu_proc.c`（`FAVOR_TOPIC` 菜单项） | 我收藏的主题列表：两个名称栏目、就地取消收藏，以及每行通向的看板 | 完成 |
| [src/roster.rs](src/roster.rs) | 661 | `src/user_list_loader.c` | 在线用户列表与发帖数统计 | 完成 |
| [src/userinfo.rs](src/userinfo.rs) | 923 | `src/user_info_display.c`、`src/user_list_display.c`（查找）、`src/bbs.c`（等级与星座表） | 一个用户的资料页，以及通向它的用户名查找 | 完成 |
| [src/userlist.rs](src/userlist.rs) | 1024 | `src/user_list_display.c`、`src/menu_proc.c`（两个花名册入口） | 线上使用者与已注册用户两页：标题、栏目、翻页，以及在两页之间走动的按键 | 完成 |
| [src/loader.rs](src/loader.rs) | 1529 | `src/section_list_loader.c` | 增量刷新任务、它顺带写出的文章缓存文件，以及每个版块那棵精华区菜单 | 完成 |
| [src/cache.rs](src/cache.rs) | 554 | `src/article_cache.c` | 渲染与读取 `var/articles/<CID>` 及其行偏移表 | 完成 |
| [src/menu.rs](src/menu.rs) | 2351 | `src/menu.c` | `menu.conf` 解析与页面跳转 | 解析/跳转完成，功能本体未完成 |
| [src/lml.rs](src/lml.rs) | 689 | `src/lml.c` | LML 标记渲染 | 完成 |
| [src/bwf.rs](src/bwf.rs) | 241 | `src/trie_dict.c` 等 | 违禁词匹配与遮蔽 | 完成 |
| [src/ipmask.rs](src/ipmask.rs) | 109 | `src/ip_mask.c` | IP 地址掩码（`202.38.64.x`） | 完成 |
| [src/privilege.rs](src/privilege.rs) | 385 | `src/user_priv.c` | 权限位与权限等级计算 | 完成 |
| [src/session.rs](src/session.rs) | 613 | 各 `BBS_*` 全局变量 | 一次会话的状态 + 在线记录 | 完成 |
| [src/login.rs](src/login.rs) | 580 | `src/login.c` | 登录、认证、用户协议（EULA） | 完成 |
| [src/bbsnet.rs](src/bbsnet.rs) | 1767 | `src/bbs_net.c`、`src/menu_proc.c`（`@mod:` 分发） | 站点表、穿梭网格、对外连接与双向转发 | 完成 |
| [src/bbs.rs](src/bbs.rs) | 872 | `src/bbs_main.c`、`src/bbs_cmd.c` | 会话自身的执行顺序（含回显接管）、`bbs_center` | 提示、菜单循环与浏览类命令完成，少数菜单动作仍只记日志 |
| [src/net.rs](src/net.rs) | 360 | `src/net_server.c` | accept、连接数限制、优雅退出 | 完成 |
| [src/ssh.rs](src/ssh.rs) | 496 | `src/net_server.c`（libssh 那一半） | 宿主密钥、密码认证、会话通道 | 完成 |
| [src/main.rs](src/main.rs) | 542 | `src/main.c`、`src/net_server.c` | 启动顺序、信号处理、`SIGHUP` 重载、退出 | 完成 |
| [src/batch.rs](src/batch.rs) | 374 | `utils/lib/db_open.inc.php` + 三个脚本里反复出现的那几行 | 一个生成工具的完整一生：参数、服务器根目录、`conf/bbsd.conf`、数据库，以及脚本自己的 stdout 文字与退出码 | 完成 |
| [src/section_list.rs](src/section_list.rs) | 149 | `utils/lib/section_list.inc.php` | 生成工具要走的那棵分类/版块树，附带调用方自己的每版数据 | 完成 |
| [src/gen_top.rs](src/gen_top.rs) | 305 | `utils/bin/gen_top.php` | `var/bbs_top.txt` 与 `var/bbs_top_menu.conf`：本周十篇最热帖子 | 完成 |
| [src/gen_section_menu.rs](src/gen_section_menu.rs) | 681 | `utils/bin/gen_section_menu.php` | `var/menu_merged.conf`：手写的 `conf/menu.conf` 后面接上六个生成的菜单块 | 完成 |
| [src/gen_ex_list.rs](src/gen_ex_list.rs) | 394 | `utils/bin/gen_ex_list.php` | 被守护进程标了记的每个版块，各写出一个 `var/gen_ex/<SID>` 精华区树 | 完成 |
| [src/bin/gen_top.rs](src/bin/gen_top.rs)、[gen_section_menu.rs](src/bin/gen_section_menu.rs)、[gen_ex_list.rs](src/bin/gen_ex_list.rs) | 30 | `php utils/bin/*.php` 那几行 crontab | 三个可执行文件：调用三个生成工具，把脚本的退出码原样交回给 shell | 完成 |

另有 [src/daemon.rs](src/daemon.rs)（415 行，命令行与后台化）、[src/log.rs](src/log.rs)（497 行，两个日志文件）、[src/state.rs](src/state.rs)（407 行，加载器发布、会话读取的共享状态）。

合计 31,177 行（含测试，测试就写在被测模块里）。共 462 个测试：库 456 个、可执行文件 6 个。其中 448 个只读代码
仓库里的东西，另外 14 个默认跳过，因为它们要连真实数据库、读真实生成的文件。

> 数字会随开发变化。以 `cargo test --lib` 的输出为准。

---

## 6. 哪些能用了，哪些还没有

如实交代进度是这套方法的一部分：说“能用”的都有测试覆盖，跑不起来的东西绝不说成能用。

### 6.1 ✅ 已完成（且有测试）

1. **配置解析**——`conf/bbsd.conf` 的切分方式与 C 完全相同（空格、制表符、CR、LF 为分隔；键名不区分大小写）。
2. **显示宽度计算**——中日韩全角字符算 2 列，控制序列算 0 列，规则与 C 的 `str_process.c` 一致。
3. **按键解析**——`ESC [ A` 被识别为“上箭头”；跨包被切断的序列会整段丢弃并报成裸 `ESC`，与 `igetch` 相同。
4. **字符编码**——UTF-8 与 GBK 都支持；GBK 字符在包边界被切断也能正确拼回。
5. **行输入（`str_input`）**与**分页器（`display_data`）**——密码不回显（`NOECHO` 提示打印 C 的
   `*`/`**` 掩码）、按 `q` 退出等按键行为与 C 一致。
6. **MySQL 查询**——凭据校验、版块与帖子索引、在线列表、经验值累计等。
7. **版块/帖子/用户的内存池**（替代共享内存）**以及增量更新逻辑**。
8. **`menu.conf` 的语法解析**、条目光标移动、按权限过滤。
9. **LML 渲染**与**违禁词遮蔽**。
10. **权限模型**（`user_priv.c`）与 **IP 掩码**（`ip_mask.c`）。
11. **登录流程**：三次尝试、`guest` 与 `new` 的特殊处理、防爆破限流（按 /24 网段与按账号）、用户协议重新确认、
    以及“版主和管理员必须走 SSH 登录”的规则。
12. **服务器本体**：`conf/bbsd.conf` → 共享状态 → 加载任务 → 两个监听端口，含 C 的命令行、
    `--foreground`/`--display-log`、在建 runtime 之前完成的 `fork()`、两个日志文件与 `log_restart()`、
    `SIGHUP` 重载，以及先等 15 秒、再给 1 秒的 `SIGTERM` 收尾。周期任务用的就是做完初始加载的那个
    loader——C 那边靠 `fork()` 把游标交给了子进程。`last_article_op_log_mid` 是一次扫描够得着
    `bbs_article_op` 的前提，另起一个空白 loader 的循环只会不停地追加新帖，老帖的改动一条也看不见。
13. **连接数限制**：总量 `bbs_max_client` 与单 IP `bbs_max_client_per_ip`，计数方式与 C 父进程一致，
    日志行也一致。
14. **SSH**：用 russh 复用同一批宿主密钥，只提供密码认证、同样三次尝试、一条会话通道，
    会话里跑的就是 telnet 那条 `bbs_main`。
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
    列表上的 `m` 则是 `article_excerption_set`（`article_op.c:34-269`），也就是精华标记的开关：先 `FOR UPDATE`
    锁出那一行的五列，再改标记；取消时顺带清掉 `gen_ex`/`static` 与 `ex_file`，附件的归属随之转移，作者的
    经验值按原版主题 50、转帖 20、回复 10 计费，取消时同样数额倒扣，最后写进 `'E'`/`'O'` 那条操作记录，
    提交之前再改共享池里的标记——C 提交失败时会把池子的标记退回去了。它一个字符都不画：行首那个标记就是
    全部的回应；过不了 `S_POST | S_MAN_S` 的用户按下去，列表原样重画，没有提示也没有响铃。
19. **全屏编辑器**（`editor.c`）：插入与改写、行与窗口的行列运算、`Ctrl-K`/`Ctrl-Y` 删除、退格合并行、
    宽字符整体删除、`Ctrl-W`/`Ctrl-X` 不重绘直接退出、状态行，以及 `MAX_EDITOR_DATA_LINES` 上限。
20. **生成文章缓存。** [`cache::generate`](src/cache.rs) 与 C 的
    `article_cache_generate` 逐字节一致，加载器在每收录一帖、每重放一个 `'M'` 之后都会调用它
    （`section_list_loader.c:327`、`:542`），所以 `var/articles/<CID>` 会自己出现——帖子落库后一秒之内，与 C 相同。
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

1. **写入路径背后的界面**：用户资料编辑器、统计、`display_article_meta`。
   这些按键目前会记一条 `... is not implemented`，而不是假装能用。
2. **指向上面这些处理函数的一级菜单条目**：`bbs_do_choice` 只记日志并返回，所以上位菜单仍有只会记日志的键。
   `LIST_ONLINE_USER`、`LIST_USER`、`RunMBEM`、`FAVOR_TOPIC`、`LIST_EX_SECTION` 与 `VIEW_EX_ARTICLE` 已经接上：
   前两个连同它们通向的资料页与用户名查找，后两个连同版块的精华区树与从树里读出的一篇帖子（树在 `src/browse.rs`
   浏览、由各版块的菜单在 `src/loader.rs` 装载）。还在只记日志的是 `EDIT_INTRO`、`EDIT_SIGN`、`DATAMGR` 与
   `M_FAVOR_SECTION`。
3. **Phase 2 的收尾**：GBK 的边角情况、SSH 的窗口大小（有意忽略，理由见 `src/ssh.rs` 文件头），
   以及与 C 就 Phase 1 全部界面做逐屏对照。

所以目前仓库的状态是“**服务器能跑，游客可以浏览版块、读帖子，写入路径已接好并有单元测试**。
第一次真的发帖请先在测试数据库上做。

---

## 7. 搭建开发环境

### 7.1 需要的东西

| 项目 | 版本 | 说明 |
| --- | --- | --- |
| Rust | 1.96 或更新 | 本 crate 使用 `edition = "2024"` |
| MySQL | 8.x | 存放论坛数据的那个库 |
| Linux | — | 开发与验证都在 Linux 上进行 |
| locale | `en_US.UTF-8` | `locale -a` 里必须有它：服务器在写出任何文章缓存之前会安装这个 locale，与 C 一样，缺了就拒绝启动 |

`cargo` 是 Rust 自带的构建与测试工具。以下命令都在 `lbbs-rust/` 目录里执行。

```bash
cd lbbs-rust
cargo build            # 编译库和可执行文件
cargo test             # 438 个不需要数据库的测试（另有 14 个见 7.2）
```

正常情况长这样：

```
test result: ok. 432 passed; 0 failed; 14 ignored; 0 measured; 0 filtered out
...
test result: ok. 6 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out
```

> **本项目的开发环境里没有装 `cargo fmt`。** 缩进（4 空格）、命名等格式靠手工维持，
> 所以提交前请自己读一遍 diff，不要指望工具替你排版。

### 7.2 使用真实数据库、真实配置文件的测试

有 14 个测试标了 `#[ignore]`，因为它们会连真实数据库，或者需要真实生成的配置文件。这些文件现在就在本仓库里
（见第 8 节），只要把位置告诉它们即可；`LBBS_REF_CONF` 会决定会话测试从哪个根目录读 `var/`，所以要写绝对路径：

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

本仓库带的配置监听 telnet `2323`、SSH `2322`（来自 `bbs_port` 和 `bbs_ssh_port`），与参考用的 C 目录一致；
模板文件里写的还是 C 的 `23` 和 `22`。
用 telnet 连接时请加 `-c`：服务器会要求客户端停止本地回显，加 `-c` 可以避免协商完成前字符被显示两次。

> **同一个数据库同时只跑一个前端。** 本仓库的 `bin/bbsd` 和 C 版的 `bbsd` 写的是同样的
> `user_online`、`visit_log` 行；一起跑不会损坏数据，但在线列表和计数会重复。

游客会话能一路走完：两个提示、欢迎页、登录、上位菜单、讨论区组、版块、帖子列表，以及一篇真实帖子。
[tools/guest_smoke.py](tools/guest_smoke.py)
驱动的就是这条路径，它停在权限门口，因此不会写任何东西；运行方式是
`python3 tools/guest_smoke.py 2323`。

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

---

## 8. 配置文件与数据文件的位置

本仓库自带一个服务器根目录，因此不需要手边有 C 目录，就能替代 `bbsd` 或与之并行运行。连接过程中要读的东西
都在树里；不进 git 的只有属于某台机器、属于某个人的东西——密码和主机密钥。

```
lbbs-rust/                      ← 服务器根目录（见 7.3）
├── bin/                       ← 放编译好的二进制；不进 git
├── conf/
│   ├── bbsd.conf.default      ← 主配置模板
│   ├── bbsd.conf              ← 你自己那份，含凭据；不进 git，见下文
│   ├── bbsnet.conf            ← 本站认识的其他站点（另有 .default）
│   ├── badwords.conf          ← 违禁词（另有 .default）
│   ├── menu.conf              ← 菜单定义，是 var/menu_merged.conf 的来源
│   ├── ca_cert.pem            ← MySQL 的 TLS 校验（C 版在用，所以我们也用）
│   ├── lbbs.service           ← systemd 单元，以及配合日志轮转的 lbbs.logrotate（另有 .in 模板）
│   └── ssh_host_*             ← SSH 主机密钥：自己生成，绝不提交，见下文
├── data/                      ← 界面要显示的十一个文本：welcome、goodbye、eula、register、
│                                 login_error、license、copyright、version、active_board、read_help、
│                                 editor_help（启动时预载的第十二个文件是 var/bbs_top.txt）
├── var/
│   ├── menu_merged.conf       ← 守护进程真正读取的菜单集
│   ├── bbs_top_menu.conf      ← 生成的十大热门菜单
│   ├── bbs_top.txt            ← 登录成功后立刻显示的画面
│   ├── gen_ex/<SID>           ← 单个版块的精华区树，由 gen_ex_list 重写
│   ├── articles/<CID>         ← 帖子正文——这是数据不是配置，不进 git，见下文
│   └── section_aid_loc/<UID>  ← “最后读到哪”的状态，随浏览写入
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
- **`var/articles/` 是内容，由服务器自己生成。** `article` 表存的是索引，读者看到的正文来自
  `var/articles/<CID>`，文件不存在就是硬错误，日志写作 `article_cache_load(aid=…, cid=…) error`，与 C 一致。
  加载器在收录新帖时就把这些文件写出来（[`cache::generate`](src/cache.rs)
  就是那套渲染逻辑的移植，且与 C 逐字节一致），所以一个空目录会自己从数据库里填满——测试论坛的 28000 个文件、
  130 MB 大约两秒——不必再从别的目录复制。它照样不进 git，`.gitignore` 已经替你做了这件事。

Rust 里这些路径集中放在 [src/paths.rs](src/paths.rs)，
再相对于 [`Session`](src/session.rs) 持有的服务器根目录解析。

`conf/bbsd.conf` 的键名（与 C 完全一致，含义一致、怪癖也一致）：

| 文件里的键 | Rust 字段 | 含义 |
| --- | --- | --- |
| `bbs_id` | `bbs_id` | 论坛内部名称（例如 `LBBS-devel`） |
| `bbs_name` | `bbs_name` | 界面显示名（例如 枫林在线） |
| `bbs_server` | `bbs_server` | 域名 / 对外公布的地址 |
| `bbs_address` | `bbs_address` | 监听地址 |
| `bbs_port` | `telnet_port` | telnet 端口（参考配置里是 2323） |
| `bbs_ssh_port` | `ssh_port` | SSH 端口（参考配置里是 2322） |
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
- 连接 MySQL 时会通过 `ca_cert.pem` **校验服务器证书**，因为 C 客户端就是这么做的。
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
| 4 | 欢迎画面 | 以 `display_file(..., 2)` 显示 `data/welcome.txt` | [`Session::display_file`](src/session.rs#L217) ✅ |
| 5 | “请输入帐号” | `bbs_login`，最多 3 次 | [`login()`](src/login.rs#L160) ✅ |
| 6 | 输入 `guest` 或 `new` | 试用账号 / 注册说明 | ✅（`new` 显示说明后结束会话） |
| 7 | “请输入密码” | `check_user` | [`check_user()`](src/login.rs#L235) ✅ |
| 8 | 失败次数过多被挡住 | /24 网段 10 分钟内 10 次；单账号 3 次 | ✅（`ipmask::subnet_pattern` + `db::count_errors_by_*`） |
| 9 | `您目前无权登陆...` | 原样复现，包括“先写日志再判断”的顺序 | ✅ |
| 10 | “用户协议已更新” | `user_update_agreement` | [`update_agreement()`](src/login.rs#L418) ✅，两种传输都会调用 |
| 11 | 上线登记、访问计数 | `user_online_add`、`visit_log` | [`Session::online_add`](src/session.rs#L162) ✅ |
| 12 | `var/bbs_top.txt` + 上位菜单 | `display_file(VAR_BBS_TOP)` → `bbs_center` | [`bbs_main`](src/bbs.rs) ✅，含十秒一次的 `data/active_board.txt` 面板 |
| 13 | 选版块 → 选帖子 → 阅读 | 菜单条目 + `display_file` + `section_list_*` | 菜单解析与跳转 ✅，[`browse`](src/browse.rs) ✅ —— 这正是 `tools/guest_smoke.py` 走的那条游客路径 |
| 14 | 发帖、回复、修改、删除 | 编辑器 + 写 `var/articles/` + 更新数据库 + 违禁词 | [`editor`](src/editor.rs) ✅，[`post`](src/post.rs) ✅（每个动作一个事务，提交时过滤违禁词）；游客在任何 SQL 之前就被拒绝 |
| 15 | 经验值结算与告别 | `user_online_exp` + `data/goodbye.txt` | [`Session::logout`](src/session.rs#L244) ✅ |
| 16 | 从在线列表消失 | `user_online_del` | [`Session::cleanup`](src/session.rs#L260) ✅ |

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
| 用 `SIGUSR1` 刷新内存池 | `Loader::run` 周期性刷新 | 用任务替代信号 |
| `editor.c` 每次编辑只重新拆分改动的那条数据行，再挪动显示指针数组 | 同样只重排那一行（[`EditorData::recount_line`](src/editor.rs)） | 最早的移植每敲一个字符就把整个文档重排一遍，黏贴长帖时开销随长度平方增长、把一个 CPU 核跑满，而 C 几乎不耗 CPU |
| 显示行数已经占满 [`MAX_EDITOR_DATA_LINES`](src/paths.rs) 之后再编辑，需要新行的那部分内容就地丢掉：被改的那一行截短，并在保留的末行末尾粘一个 `'\n'`（`:366-387`）；若改动在更前面，则把数组最后一行削到少一格（`:447-458`） | 同处在到达上限时 | 最早的移植改成拒绝这次编辑，于是一篇写满的文章再也输不进东西；C 在这里是静默截掉尾部。只有一种情形不复现：在一份占满数组的文档里删除最后一行，C 数出来的行数比文档实际多一行——那是它数组里一条不再维护的残留记录，画出来是空的，而移植画的是真实存在的那一行；两种算法存盘的字节一致。 |

### 10.2 原样复现的 C 语言“怪癖”

移植中最危险的事，就是“好心”把原来的 bug 修掉：显示和数据都会变，网页端就会和我们对不上。
因此下面这些行为是**逐字复现**并用测试钉住的。

| 原始行为 | 位置 | 为什么不能“顺手修好” |
| --- | --- | --- |
| 权限列表的折半查找是坏的：取决于版块加入的先后顺序，已存下的版块权限可能**再也查不到** | [`privilege.rs`](src/privilege.rs) | 一旦“修好”，某些人的权限会凭空改变 |
| 列表为空时会去检查第 0 个槽位（SID 0，即全局权限位） | 同上 | 同上 |
| `ip_mask` 会在**第一个被掩码的字节处截断字符串**，所以带第五段的地址尾部会消失 | [`ipmask.rs`](src/ipmask.rs) | 限流用的 `LIKE '202.38.64.%'` 正是依赖这个表达式 |
| 用户名/密码**格式校验阶段一条 SQL 都不发**，所以格式错误的尝试不会被记录 | [`login.rs`](src/login.rs) | 网页端的解锁功能依赖这些计数 |
| 账号被停用（`p_login = 0`）时，登录日志**先提交**再拒绝 | 同上 | 否则访问计数会变 |
| 走 EULA 流程时既不更新访问计数，也不登记在线 | 同上 | 同上 |
| `moveto` 遇到负数会带符号输出；行号为负时只输出一个 `\r` | [`Terminal::moveto_signed`](src/term/mod.rs#L231) | 菜单光标位置依赖这个行为 |
| 跨包被切断的按键序列会被丢弃并报成 `ESC` | [`Terminal::read_key`](src/term/mod.rs#L275) | 分页器的按键手感必须完全一致 |
| `S_DEFAULT = 0x3`（可看列表 + 可得经验值），**不含发帖位** | [`privilege.rs`](src/privilege.rs) | 让新账号“更方便”就会与网页端不一致 |
| 被对方**拒绝**的连接不会立刻报出来，要等十秒预算走完，然后报 `连接超时！` | [`bbsnet::connect`](src/bbsnet.rs) | C 的等待循环在“可写但没连上”的套接字上继续空转，进度条期间按下的键也被连接吃掉；提前失败会同时改掉屏幕上的字和真正送到站点的按键 |

真要对上述任何一条做修复，应当与数据、网页端一起作为一次协同变更，绝不能在移植里悄悄改掉。

### 10.3 1.7.12 这一轮参考端的修改在 Rust 版里对应什么

`v1.7.11..b7a8b16` 一共十八个提交——十七个修复加一个版本号。逐 hunk 对照之后的结论是：**没有一个需要在
Rust 里做功能改动**。它们泄漏、忘了解锁、少算边界、把用户文本当成 `printf` 格式，而这些代码在本项目里是
owned buffer、RAII guard 和绑定参数。每个修复*说明*了什么，已经写进对应 C 文件的替代模块，下表只是索引。

| 参考端提交 | C 里的问题 | Rust 版在哪儿交代了 |
| --- | --- | --- |
| `6012565` | `io.c` 往 `char tmp[]` 里累积 ESC 序列却没有上界，修复后停在 `sizeof(tmp) - 1` | [`term/mod.rs`](src/term/mod.rs) —— 解码器消费 `Vec<u8>`，不完整就报 partial，没有会被写爆的定长累加器 |
| `de5fa0f` | `article_cache.c` 的错误路径漏掉 `render_len`、一个 `fd` 和两次映射 | [`cache.rs`](src/cache.rs) —— `File` 与 `Vec` 在任何 return 上都会自行释放 |
| `a2f6c10` | `article_post.c:276`、`:1149`：`len_content` 累加的是 `snprintf` *想*写的长度；`article_reply` 忽略 `lml_render` 的负返回 | [`post.rs`](src/post.rs) 头部的“页脚”那一条 |
| `37dfc47` | `screen.c` 用 `sizeof(MENU_ITEM)` 走 screen 池，上限也误用 `menu_item_count` | [`menu.rs`](src/menu.rs) —— `set.screens.len() >= MAX_MENUS` 上方的注释 |
| `c996502` | `lml.c:482-486` 把长度等于 `LML_TAG_PARAM_BUF_LEN` 的参数拷进同样长度的缓冲区，覆盖掉调用方的 NUL | [`lml.rs`](src/lml.rs) —— 参数拷贝处的钳制 |
| `a02a212`、`c6444e2` | `db_open` 有三条失败路径没 `mysql_close`；`data_file_mgr.c` 对 `MAP_FAILED` 调了 `munmap` | [`db.rs`](src/db.rs)（连接池没有句柄可漏）、[`state.rs`](src/state.rs)（根本没有映射） |
| `b2e764c` | `bwf.c` 把零长度的行当成空行；`user_list.c` 读 `row[3][0]` 少了邻居都有的 NULL 判断 | [`bwf.rs`](src/bwf.rs) —— 空行跳过处的注释；[`db.rs`](src/db.rs#L334) —— `gender` 解码成不可能为 NULL 的 `String`，因为该列是 `char(1) NOT NULL DEFAULT 'M'` |
| `51b1c7f` | 两处 `prints(buffer)` 让用户文本里的 `%` 被当成格式 | [`Terminal::print`](src/term/mod.rs) —— Rust 的 `print` 根本不接收格式 |
| `d7f2f5d` | `section_list_loader.c:145-152` 跳出版块循环时仍持着写锁 | [`loader.rs`](src/loader.rs) —— `load_section_config` 的文档；这里的 guard 不可能活过一趟刷新 |
| `0e5224a` | `editor.c` 清 `p_display_lines[]` 时每个指针只清 1 字节，回收来的池节点因此留着 7/8 的脏行 | [`editor.rs`](src/editor.rs) —— `recount()` 由数据行重算显示行，脏尾巴撑不过一次编辑 |
| `336b670` | `str_process.c` 先写第 `line_offsets_count` 行的宽度、后判边界，行号多的文件会写越偏移数组 | [`text.rs`](src/text.rs) —— `split_data_lines` 先判后截，由 `wrapped_respects_line_cap` 钉住 |
| `038dd6b` | 版本号 | 本节 |
| `4553b19` | `article_favor.c:145`、`:211` 往 `sql_del` 里写前缀却按 `sizeof(sql_add)` 计长——确实是笔误，只因两个缓冲区同为 `SQL_BUFFER_LEN` 才没出事 | [`db.rs`](src/db.rs#L772) —— `QueryBuilder` 直接把前缀拼出来，没有“拿错变量的缓冲区大小”这回事 |
| `ebaa144` | 共享内存里跟在文本之后的 `long[]` 偏移表落在 `data_len + 1`，一般并不对齐；新增的 `shm_offsets_offset()` 把长度计算、写入端、读取端统一向上取整 | 本项目从不构造那种布局：[`state.rs`](src/state.rs) 存文件字节，[`screen.rs`](src/screen.rs) 按会话折行（`file_loader.c:88` 那两处引用正是被挪走的这一行） |
| `37b0660` | `log.c:114-117`：`log_printf` 照级别挑一个 `FILE *` 就写，`log_begin()` 之前写日志就是空指针解引用；现在退回 `stderr` | [`log::begin`](src/log.rs#L276) 打开两个流，打不开就终止启动，`main.rs:134` 在任何会话或加载任务之前就把 subscriber 装好——这里没有“还没打开”的状态可退 |
| `65ce0f7` | trie 字典自测对负的 `int64_t` 做左移（`TEST_VAL << i`），这在 C 里是 UB | Rust 里有定义，而 C 用 trie 存的那几张表在这里是 `HashMap`（[`board.rs`](src/board.rs)、[`roster.rs`](src/roster.rs)）；参考端那个自测没有对应物 |
| `b7a8b16` | `editor.c:395-414`：三次 `memmove` 直接按 `display_line_total - last_display_line - 1` 平移显示行，计数一脏就变成巨大的 `size_t`，前后两头越界；修复要求 `display_line_total >= last_display_line + 1`，否则把这条意外记进日志 | [`editor.rs`](src/editor.rs) —— `EditorData::insert` 改数据行、`recount()` 重算显示行，没有需要平移的平行数组，那个负长度在这里没有形状 |

**最后这五个就是上一轮同步时还没提交的那批改动。** 其中两个会挪动引用里的行号：`b7a8b16` 在
`editor_data_insert` 里加了八行，[`editor.rs`](src/editor.rs) 里那个
hunk 之后的 `editor.c` 引用一律 +8；`ebaa144` 在文件分页器引用的 `split_data_lines` 之上加了七行，把它从
`file_loader.c:81` 挪到 `:88`。这些都不是照算术改的：每个新行号都先确认它指着的 C 文本与旧行号一致——正因
如此 `editor.c:344`、`:348-349` 原地不动，插入的判断之后那些才挪。脚本判不下来的只有 `file_loader.c` 那一行
（它正落在改动的 hunk 里），按 `git show b7a8b16:src/file_loader.c` 手工重定。

---

## 11. 在本项目里写代码的规矩

- **缩进 4 个空格。** 不依赖 `cargo fmt`（本机没装）。
- 每个模块头部必须写明移植自哪个 C 文件的哪些行，并带一节 `# Deviations from C`。
  **两者缺一的移植会被退回。**
- C 的常量（`BBS_max_section = 200`、`USERNAME_MAX_LEN = 12` 等）统一放进 `paths.rs`，不要写裸数字。
- 不需要数据库的测试用 `#[test]` / `#[tokio::test]`；要连真实库的必须 `#[ignore]`，并靠 `LBBS_REF_*` 环境变量取路径。
- 离线测试需要一个 `Session`？用 [`session::offline_session()`](src/session.rs)：
  它构造的是延迟连接池，不会真的建立连接，也不发任何查询。
- 在 `async fn` 里到处传 `&mut Session` 时注意借用冲突。例如
  [`Session::display_file`](src/session.rs#L217) 是递归的——
  分页器可以请求显示帮助，而帮助本身又是显示文件——所以它的 future 用 `Pin<Box<dyn Future<...>>>` 装箱；
  普通的 `async fn` 无法拥有递归的返回类型。
- 新增 SQL 之前，先读 C 里同一件事的上下文，**尤其是何时 commit**。
- 输出先缓存在 `Terminal`，只有 `flush`（对应 C 的 `iflush`）才会真正发到线上。
  读取按键之前一定会先 flush，所以提示语不会被吞掉。

### 11.1 先记住这几个类型就够了

| 类型 | 文件 | 一句话 |
| --- | --- | --- |
| `AppState` | `state.rs` | 所有连接共享的东西（配置、数据库池、版块/帖子/用户内存池） |
| `Session` | `session.rs` | 一次连接：uid、用户名、权限、对端地址、当前动作、时间戳 |
| `Terminal` | `term/mod.rs` | 这条连接的带缓冲输入与输出 |
| `Config` / `DbConfig` | `config.rs` | 解析后的 `bbsd.conf` |
| `Db` | `db.rs` | sqlx 连接池；内部是 `Arc`，所以 `Clone` 很轻 |
| `UserPriv` | `privilege.rs` | 一个人的等级、全局权限位、各版块的权限位 |
| `MenuSet` / `MenuItem` / `Action` | `menu.rs` | 菜单条目，以及按下某个键要执行哪个功能 |
| `Board` | `board.rs` | 版块、帖子、用户的内存副本 |

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
答：C 有“定宽 (F) / 变宽 (V)”两种选择，宽度算式不同。请统一走 `Session`/`Terminal` 的 `fixed_width`，
不要直接调用 `text::display_width`。

**问：输入密码时屏幕上出现了明文。**
答：不应该出现。会话一开始就送出 `IAC WILL ECHO, IAC WILL SGA`
([`screen::set_input_echo`](src/screen.rs))，客户端据此不再自己回显；
`NOECHO` 提示接着每字符打印一个 `*`，回车时再擦掉。若明文照旧可见，那就是传输层在第一个提示之前
没把六个协商字节写出去——只有掩码挡不住客户端自己的回显。

**问：我想和本地已有的网页版一起试用。**
答：把 `conf/bbsd.conf` 指向**备份库或测试库**，`db_password` 只留在这个文件里（不提交、不打印）。

**问：帖子正文在哪？**
答：`var/articles/<CID>`，由 `article.CID` 索引。C 用 `fopen` 打开；移植版同样是直接打开文件
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
| 已完成 | 监听：裸 TCP + russh SSH、连接数限制、真正的 `main.rs`、EULA 接入 | `cargo run -- --foreground` 可以接受连接 |
| 已完成 | 对论坛数据库做联机冒烟测试：游客登录、菜单、SSH 认证、重载、重开日志 | 同一批界面表现得和 C 一致 |
| 已完成 | 打通浏览功能：`list_section`、帖子列表、阅读、`LOCATE_ARTICLE` | 按菜单键能进版块，联机游客会话能读到一篇真实帖子 |
| 已完成 | 写入路径：新帖/回复/修改/删除、编辑器、索引更新、缓存生成、提交时过滤违禁词 | 有单元测试覆盖，且联机游客按 `^P` 在任何 SQL 之前被拒绝 |
| 已完成 | `utils/bin` 的三个 PHP 生成工具改成可执行文件：`gen_top`、`gen_section_menu`、`gen_ex_list`（见 7.4） | 对着真实库，它们写出的文件与 PHP 逐字节一致（见 6.2） |
| 下一步 | 在**测试库**上完整走一遍真实发帖，再依次验证回复/修改/删除 | 网页端能看到移植版写出的帖子，头信息与缓存文件和 C 一致 |
| 再下一步 | 剩下的菜单条目：资料编辑两项、`DATAMGR`、`M_FAVOR_SECTION` | 上位菜单里不再有“只写日志”的按键 |
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
