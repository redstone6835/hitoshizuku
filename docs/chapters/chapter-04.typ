#import "../config.typ": project-name
#import "../styles/diagram.typ": flow-arrow, flow-node, layer-card
#import "../styles/figure.typ": continued-table, figure-caption, pseudo-sample
#import "../styles/tokens.typ": handoff-fill, soft-fill, stable-fill, warm-fill

= 第四章 虚拟文件系统与 POSIX 兼容层

在第三章中，设备模型把硬件能力整理为设备能力，并通过投影层把一部分设备能力落实到 `/dev`、sysfs 系统文件系统和 procfs 进程文件系统等用户可见视图中。到了本章，问题从设备能力的发布转向了用户态接口的统一。用户程序不应直接理解块设备队列、字符设备缓冲区、管道环形队列或进程状态表。它需要的是一套稳定的 POSIX 文件接口。虚拟文件系统承担这个边界，把多种异构资源整理为路径、文件描述符和文件操作。

VFS 面对的异构性不亚于设备模型。磁盘文件有持久化数据和目录层级。tmpfs 内存文件系统的文件完全位于内存中。procfs 进程文件系统的文件在读取时动态生成文本。devtmpfs 设备文件系统节点承接设备能力的用户态投影。管道没有目录中的长期身份，却仍然需要一个文件对象来表达读端和写端。若 VFS 只关注普通文件，系统调用层就会被迫为每类资源开辟专用路径。若 VFS 试图把所有资源压成相同的读写语义，又会损失设备控制、挂载边界、共享偏移和 `unlink-but-open` 等关键语义。

我们在 VFS 中采用的核心思路，是把稳定 ABI 与内部对象模型分开。路径解析、权限检查和文件描述符管理由 VFS 统一处理。具体文件系统和设备适配器只实现自己负责的对象语义。打开动作是二者的交接点。它把一个路径名转换为带有访问能力的文件对象，并把后续操作限制在这个已冻结的能力边界内。这样做既能满足 POSIX 的兼容要求，也能让 tmpfs 内存文件系统、devtmpfs 设备文件系统、procfs 进程文件系统和 pipefs 管道文件系统按各自的内部规律演化。

== 4.1 设计目标与约束

VFS 的设计目标可以分为四类。第一是命名空间稳定。路径名必须能在目录树、挂载点和符号链接之间得到确定解析，并且要遵守进程可见根与当前工作目录。第二是对象身份稳定。一个文件可以被重命名，可以存在多个硬链接，也可以在删除后继续被已打开的文件描述符访问。第三是能力边界稳定。打开时通过权限检查后，后续读写不能再被路径替换影响。第四是扩展边界稳定。新增文件系统或设备节点适配器时，不应修改系统调用层和文件描述符表。

#continued-table(
  "4-1",
  [VFS 的设计目标],
  (1.05fr, 2.05fr, 2.25fr),
  (
    table.cell(fill: soft-fill)[#text(weight: "bold")[目标]],
    table.cell(fill: soft-fill)[#text(weight: "bold")[设计含义]],
    table.cell(fill: soft-fill)[#text(weight: "bold")[关键约束]],
  ),
  (
    table.cell(fill: warm-fill)[命名与对象分离],
    table.cell(fill: warm-fill)[目录项描述路径分量和父子关系，索引节点描述文件对象本体。],
    table.cell(fill: warm-fill)[`rename`、`link` 和 `unlink` 都不能破坏已打开文件的对象身份。],
    table.cell(fill: soft-fill)[打开能力冻结],
    table.cell(fill: soft-fill)[文件对象在 `open` 时保存访问模式和凭据快照。],
    table.cell(fill: soft-fill)[后续 I/O 只检查文件对象能力，避免路径 TOCTOU 窗口。],
    table.cell(fill: handoff-fill)[挂载边界显式],
    table.cell(fill: handoff-fill)[挂载命名空间维护挂载索引，路径解析跨越挂载点时切换挂载对象。],
    table.cell(fill: handoff-fill)[`..` 必须同时处理目录父节点和挂载父节点，且不能越过进程根。],
    table.cell(fill: stable-fill)[驱动扩展收敛],
    table.cell(fill: stable-fill)[文件系统实现索引节点操作集（`InodeOps`）与文件操作集（`FileOps`），设备节点通过投影器注入节点载荷。],
    table.cell(fill: stable-fill)[系统调用层只消费 VFS 通用接口，不下沉到具体文件系统或设备类型。],
  ),
  kind: "表",
  continuation-kind: "续表",
  align: (left, left, left),
)

这些目标中，最容易被低估的是打开能力冻结。POSIX 程序常常先打开路径，再把文件描述符交给另一个线程或子进程。此后这个描述符应当代表一次已经完成的授权，而不是每次读写都重新追踪路径。路径可以被其他任务重命名或替换，文件权限也可以改变。若读写时重新解析路径，程序会得到不稳定的对象。若读写时重新检查路径权限，攻击者就可能在检查和使用之间替换目录项。我们把权限检查和路径解析集中在打开阶段，后续调用只面对文件对象。

== 4.2 总体分层结构

VFS 被拆成五个层次。最上层是 POSIX 系统调用翻译层，它把 `openat`、`read`、`write`、`stat`、`mount` 和 `ioctl` 等调用转换为 VFS 请求。第二层是 VFS 上下文和文件描述符表，它保存进程可见根、当前目录、凭据、umask 和已打开文件。第三层是路径解析、目录项缓存、索引节点管理和挂载命名空间。第四层是具体文件系统驱动。第五层是设备、内存和块层后端。

#figure(caption: figure-caption("图", "4-1", [VFS 的单向信息流]))[
  #layer-card("POSIX 系统调用层", [`openat`、`read`、`write`、`close`、`stat`、`mount`、`ioctl`、`mmap`], fill: soft-fill)
  #flow-arrow(label: "转换为 VFS 请求")
  #layer-card("进程 VFS 状态", [VFS 上下文、文件描述符表、凭据对象、umask 和资源限制], fill: soft-fill)
  #flow-arrow(label: "提供起点与能力表")
  #layer-card("VFS 核心对象", [路径解析、目录项缓存、索引节点、超级块、挂载命名空间和文件对象], fill: handoff-fill)
  #flow-arrow(label: "委托具体语义")
  #layer-card("文件系统与投影层", [tmpfs 内存文件系统、devtmpfs 设备文件系统、procfs 进程文件系统、sysfs 系统文件系统、pipefs 管道文件系统和设备文件投影器], fill: warm-fill)
  #flow-arrow(label: "访问后端资源")
  #layer-card("设备、内存与块层", [设备能力、页缓存、物理页、块设备请求、网络和管道缓冲区], fill: stable-fill)
]

这条信息流保持单向依赖。系统调用层依赖 VFS。VFS 依赖文件系统驱动和设备投影。具体设备不反向依赖系统调用层。devtmpfs 设备文件系统会观察第三章中的设备能力注册表，但设备探测是否成功不由 `/dev` 节点创建结果决定。这个边界使启动顺序更宽松。设备可以先注册设备能力，devtmpfs 设备文件系统在挂载后再补齐节点。VFS 也可以在没有某类特殊文件系统时继续管理普通文件。

== 4.3 索引节点与超级块

索引节点（`Inode`）表达文件系统中的对象身份。它包含跨文件系统唯一的索引节点编号（`InodeId`）、对象类型、设备号、块大小和指向索引节点操作集（`InodeOps`）的操作表。可变元数据集中在索引节点元数据（`InodeMeta`）中，包括大小、权限、所有者、时间戳和硬链接计数。大小和链接计数同时保存为原子镜像，供热路径快速读取。超级块（`Superblock`）表达一次文件系统实例，它保存文件系统类型、实例标识、设备号、根索引节点、根目录项和索引节点缓存。

#pseudo-sample("4-1", [索引节点与超级块的职责边界], kind: "代码")[
  ```rust
  struct InodeId {
      fs_id: u32,
      ino: u64,
  }

  struct InodeMeta {
      size: u64,
      nlink: u32,
      mode: u16,
      uid: u32,
      gid: u32,
      atime: Timespec,
      mtime: Timespec,
      ctime: Timespec,
      blocks: u64,
  }

  struct Inode {
      id: InodeId,
      kind: InodeKind,
      rdev: DevId,
      blksize: u32,
      meta: Spinlock<InodeMeta>,
      cached_size: AtomicU64,
      cached_nlink: AtomicU32,
      lifecycle: AtomicU8,
      ops: Arc<dyn InodeOps>,
      superblock: Weak<Superblock>,
  }

  struct Superblock {
      fs_type: FsType,
      fs_id: u32,
      dev_id: DevId,
      block_size: u32,
      root_inode: Arc<Inode>,
      root_dentry: Arc<Dentry>,
      inode_cache: InodeCache,
      ops: Arc<dyn SuperblockOps>,
  }
  ```
]

这种拆分背后的原因，是文件对象的身份和文件系统实例的身份需要分别维护。同一个超级块内部的索引节点编号可以由驱动决定。tmpfs 内存文件系统可以使用自己的分配器，磁盘文件系统可以沿用磁盘上的编号，devtmpfs 设备文件系统可以根据投影对象生成编号。VFS 只要求 `fs_id` 与 `ino` 的组合全局唯一。这样既避免全局索引节点号分配器成为竞争点，也允许驱动保留自己的内部结构。

索引节点缓存保存在超级块中，并使用弱引用。弱引用可以加速同一编号的重复查找，却不阻止对象回收。这个选择与生命周期语义有关。目录项、文件对象和正在执行的文件系统操作都可能持有索引节点的强引用。超级块的缓存若也持有强引用，就会把所有访问过的索引节点永久保活。弱引用让缓存成为索引，而不是所有权来源。查找时弱引用仍然有效则升级为强引用，失效时清理缓存并重新交给驱动创建对象。

== 4.4 目录项与路径缓存

目录项（`Dentry`）表达命名空间中的一个位置。它由父目录、分量名称和可选索引节点组成。正向目录项表示名称存在，负向目录项表示名称不存在，失效目录项表示缓存结论需要重新验证。这个结构让路径解析可以缓存成功查找，也可以缓存失败查找。对于 shell 按 PATH 搜索命令这种场景，负向缓存能减少大量重复失败查询。

#pseudo-sample("4-2", [目录项状态与分片缓存], kind: "代码")[
  ```rust
  enum DentryState {
      Positive,
      Negative,
      Invalid,
  }

  struct DentryMeta {
      name: SmallStr,
      parent: Option<Arc<Dentry>>,
  }

  struct Dentry {
      inode: Option<Arc<Inode>>,
      state: AtomicU8,
      meta: Spinlock<DentryMeta>,
  }

  struct DentryCache {
      shards: [Spinlock<DentryShard>; 16],
  }

  fn lookup_cached(parent: &Arc<Dentry>, name: &str) -> Option<Arc<Dentry>> {
      let shard = hash(parent.as_ptr(), name) % 16;
      let guard = DCACHE.shards[shard].lock();
      guard.get(parent, name).filter(|d| !d.is_invalid())
  }
  ```
]

父目录项与名称共同构成缓存键。只用字符串作为键是不够的，因为不同目录下可以有相同名称。只用索引节点作为键也不够，因为同一个索引节点可以通过多个硬链接出现在不同路径。目录项的键恰好对应路径解析的局部步骤，即在某个父目录下寻找一个名称。分片哈希表降低了并发查找时的锁竞争。名称使用小字符串（`SmallStr`）保存，短路径分量直接内联在对象中，避免为常见短名称进入堆分配路径。

负向缓存的失效策略需要谨慎。父目录发生创建、链接或重命名后，旧的“不存在”结论可能失效。VFS 在这些写操作成功后，会让相关目录下的负向条目失效。这样做没有追求全局精确失效，因为精确追踪会让每次目录写入扫描更多结构。当前策略把代价放在下一次查找上。若缓存已失效，路径解析重新进入驱动。若目录没有变化，负向结论可以直接复用。

== 4.5 路径解析与挂载跨越

路径解析从 VFS 上下文（`VfsContext`）中取得起点。绝对路径从进程可见根开始，相对路径从当前工作目录开始。解析器逐个分量前进，并在每一步处理目录项缓存、底层 `lookup` 操作、符号链接、挂载跨越和 `..` 回退。最后一个分量还需要结合打开标志决定是否允许缺失、是否跟随符号链接，以及是否必须是目录。

#pseudo-sample("4-3", [路径解析的核心控制流], kind: "代码")[
  ```rust
  fn lookup(ctx: &VfsContext, dirfd: Dirfd, path: UserPath, flags: LookupFlags)
      -> Result<LookupResult, Errno>
  {
      let mut cursor = choose_start(ctx, dirfd, path)?;
      let mut symlink_depth = 0;

      for component in path.components() {
          if component == "." {
              continue;
          }

          if component == ".." {
              cursor = step_parent_without_crossing_root(ctx, cursor);
              continue;
          }

          let child = match lookup_cached(&cursor.dentry, component) {
              Some(dentry) => dentry,
              None => cursor.inode().ops.lookup(&cursor.dentry, component)?,
          };

          let mut next = enter_mount_if_needed(cursor.mount_ns(), child);

          if next.is_symlink() && should_follow(component, flags) {
              symlink_depth += 1;
              if symlink_depth > MAX_SYMLINKS {
                  return Err(ELOOP);
              }
              next = resolve_symlink_target(ctx, next, flags)?;
          }

          cursor = next;
      }

      Ok(cursor)
  }
  ```
]

`..` 的处理是路径解析中最容易出错的部分。普通目录树中，`..` 只需要回到父目录项。挂载树中，如果当前目录项是某个挂载对象的根，`..` 需要先退出当前挂载对象，再回到挂载点所在的父挂载对象。进程可见根又给这个过程加了一层限制。解析器到达进程根后必须停止回退，不能越过 `chroot` 或容器视图的边界。我们使用目录项指针和挂载对象指针判断边界，避免用字符串比较路径，后者既慢也难以处理符号链接和重复斜杠。

符号链接展开同样需要上下文。绝对目标从进程可见根重新解析。相对目标从符号链接所在目录解析。解析器维护深度计数，超过阈值后返回 `ELOOP`。这个限制看似简单，却是抵御恶意循环链接的必要条件。没有深度限制时，一个普通 `stat` 就可能把内核困在无限递归中。

== 4.6 文件对象与能力冻结

文件对象（`File`）表达一次已经打开的文件能力。它保存不可变的打开选项、可变的状态标志、调用者凭据快照、共享文件偏移、数据操作接口、打开时的目录项和所在挂载对象。路径解析和权限检查在创建文件对象之前完成。文件对象创建以后，后续系统调用通过文件描述符找到它，并根据冻结的访问能力调用文件操作集（`FileOps`）。

#pseudo-sample("4-4", [文件对象与能力冻结], kind: "代码")[
  ```rust
  struct OpenOptions {
      readable: bool,
      writable: bool,
      append: bool,
      directory: bool,
      nofollow: bool,
  }

  struct File {
      options: OpenOptions,
      status_flags: AtomicU32,
      cred: Arc<Credentials>,
      pos: AtomicU64,
      pos_lock: Spinlock<()>,
      ops: Box<dyn FileOps>,
      dentry: Arc<Dentry>,
      mount: Arc<Mount>,
  }

  fn read(file: &Arc<File>, buf: UserBuf) -> Result<usize, Errno> {
      if !file.options.readable {
          return Err(EBADF);
      }

      let _pos_guard = file.pos_lock.lock();
      let old = file.pos.load(Ordering::Relaxed);
      let done = file.ops.read_at(old, buf)?;
      file.pos.store(old + done as u64, Ordering::Relaxed);
      Ok(done)
  }
  ```
]

能力冻结解决的是路径 TOCTOU 问题。攻击者可以在权限检查后替换路径指向，也可以重命名上级目录。若读写操作每次都重新走路径，文件描述符就无法稳定代表打开时的对象。文件对象保存目录项、挂载对象和凭据快照，后续操作只面对这个对象。可变的 `O_NONBLOCK`、`O_APPEND` 等状态位则保存在 `status_flags` 中，由 `fcntl` 调整。访问模式不可变，状态标志可变，这个划分符合 POSIX 对打开文件描述的语义。

共享偏移使用位置锁串行化。单纯把偏移放进原子变量不足以满足语义，因为一次普通 `read` 包含读取旧偏移、执行 I/O、根据实际长度推进偏移三个步骤。多个线程共享同一个文件描述符时，这三个步骤必须整体有序。显式偏移接口如 `pread` 和 `pwrite` 不修改共享偏移，因此可以绕过位置锁。

== 4.7 文件描述符表与进程文件视图

文件描述符表（`FdTable`）把整数编号映射为文件对象。编号分配遵守最小可用规则。描述符级标志如 `CLOEXEC` 保存在表项中，而不是保存在文件对象中。这样同一个文件对象可以被 `dup` 复制成多个描述符，每个描述符拥有独立的关闭时继承语义，但共享文件偏移和文件状态。

#pseudo-sample("4-5", [文件描述符表的分配与批量关闭], kind: "代码")[
  ```rust
  struct FdEntry {
      file: Arc<File>,
      flags: FdFlags,
  }

  struct FdTable {
      inner: Spinlock<FdTableInner>,
  }

  struct FdTableInner {
      entries: Vec<Option<FdEntry>>,
      bitmap: Vec<u64>,
      soft_limit: u32,
      hard_limit: u32,
  }

  fn alloc_fd(table: &FdTableInner, start: u32) -> Result<u32, Errno> {
      for word_index in word_range_from(start) {
          let free = !table.bitmap[word_index];
          if free != 0 {
              let bit = free.trailing_zeros();
              let fd = word_index as u32 * 64 + bit;
              if fd < table.soft_limit {
                  return Ok(fd);
              }
          }
      }
      Err(EMFILE)
  }

  fn close_on_exec(table: &FdTable) -> Vec<FdEntry> {
      let mut guard = table.inner.lock();
      let files = guard.take_entries_with(FdFlags::CLOEXEC);
      drop(guard);
      files
  }
  ```
]

关闭路径采用锁内摘取、锁外释放。`FileOps::release` 可能进入设备驱动，也可能触发等待队列或回写。若在持有文件描述符表锁时执行这些动作，其他线程的 `open`、`dup` 和 `close` 都会被长时间阻塞。锁内只修改表结构并取出文件对象强引用（`Arc<File>`），真正的释放发生在锁外。这个原则也用于 `execve` 的 `CLOEXEC` 批量关闭。

`fork` 和 `clone` 对文件描述符表的处理由第五章的侧表钩子完成。普通 `fork` 复制描述符表，但每个条目仍然引用同一个文件对象，所以父子共享文件偏移。`CLONE_FILES` 则让两个任务直接共享同一个文件描述符表，任一方的打开和关闭对另一方可见。VFS 不需要理解任务创建细节，只提供可复制和可共享的对象边界。

== 4.8 挂载命名空间与超级块实例

挂载把一个超级块的根目录叠加到命名空间中的某个目录项上。挂载对象（`Mount`）保存超级块、挂载根、挂载位置、子挂载列表、挂载标志和打开计数。挂载命名空间（`MountNamespace`）保存一组挂载对象，并维护从挂载点到子挂载对象的索引。路径解析跨越挂载点时，通过这个索引从父挂载对象切换到子挂载对象。

#pseudo-sample("4-6", [挂载命名空间的索引结构], kind: "代码")[
  ```rust
  struct Mount {
      superblock: Arc<Superblock>,
      mount_root: Arc<Dentry>,
      location: Spinlock<MountLocation>,
      children: Spinlock<Vec<Arc<Mount>>>,
      flags: AtomicU32,
      open_count: AtomicUsize,
  }

  struct MountData {
      root: Arc<Mount>,
      mounts: Vec<Arc<Mount>>,
      by_mountpoint: Map<MountpointKey, Vec<Arc<Mount>>>,
      by_root: Map<DentryKey, Arc<Mount>>,
  }

  struct MountNamespace {
      data: Spinlock<MountData>,
  }

  fn mount_at(ns: &MountNamespace, at: LookupResult, sb: Arc<Superblock>, flags: MountFlags)
      -> Result<(), Errno>
  {
      let mount = Mount::new(sb, at.dentry, flags);
      let mut data = ns.data.lock();
      data.attach(at.mount, mount)?;
      data.rebuild_indexes_for_new_mount();
      Ok(())
  }
  ```
]

挂载标志属于挂载实例，而不属于超级块。同一个文件系统对象可以在不同位置以不同标志挂载。一个位置可以只读，另一个位置可以允许执行。若把这些标志放进超级块，就无法表达同一实例多次挂载的差异。VFS 在写入、执行和设备访问检查时读取当前挂载对象的标志，从而把文件系统内容和命名空间策略分离。

挂载命名空间使用单锁保护挂载集合。挂载和卸载并非热路径，路径解析中的挂载点查询只短暂持锁。单锁使 `pivot_root`、重新挂载和卸载的状态变化更容易保持原子。这里没有追求细粒度锁，因为挂载树操作天然跨越多个挂载对象。拆成多把锁后，正确性证明会显著变难，死锁风险也会增加。

卸载前需要繁忙检测。文件对象保存所在挂载对象，打开时递增挂载对象的打开计数，释放时递减。卸载读取计数即可发现仍有活跃文件。它不需要扫描所有进程的文件描述符表。这个设计把全局搜索转化为局部引用计数，代价是在打开和关闭路径增加一次原子操作。

== 4.9 文件系统驱动与特殊文件系统

具体文件系统通过文件系统驱动（`FsDriver`）注册到全局注册表。挂载时，VFS 根据类型名找到驱动，由驱动创建超级块、根索引节点和根目录项。索引节点上的索引节点操作集负责 `lookup`、`create`、`mkdir`、`unlink`、`rmdir`、`symlink`、`link` 和 `open` 等操作。打开成功后，驱动返回文件操作集，后续读写和控制操作就进入文件数据路径。

tmpfs 内存文件系统用内存中的目录映射保存名称关系，用页或缓冲区保存文件数据。它适合作为临时目录和共享内存后端。procfs 进程文件系统和 sysfs 系统文件系统的重点是按需生成。它们的文件内容来自读取时的内核状态格式化结果，而非持久化存储。pipefs 管道文件系统则主要服务于内核内部对象建模。管道不需要挂载到用户可见目录，但它仍然需要索引节点、文件对象和文件操作集，以便系统调用层用同一条路径处理读写、`poll` 和 `close`。

devtmpfs 设备文件系统延续第三章的投影设计。它不通过主次设备号反查驱动，而是在节点载荷中保存类型化设备能力或适配器所需的对象引用。打开 `/dev/null`、`/dev/tty`、块设备节点或 RTC 节点时，devtmpfs 设备文件系统根据节点载荷构造对应的文件操作集。设备号仍然存在，但它主要服务于 POSIX 的 `stat` 结果和用户态兼容，不再是内核内部寻找设备的主路径。

#continued-table(
  "4-2",
  [特殊文件系统的对象来源],
  (1fr, 2fr, 2fr),
  (
    table.cell(fill: soft-fill)[#text(weight: "bold")[文件系统]],
    table.cell(fill: soft-fill)[#text(weight: "bold")[对象来源]],
    table.cell(fill: soft-fill)[#text(weight: "bold")[VFS 边界]],
  ),
  (
    table.cell(fill: warm-fill)[tmpfs 内存文件系统],
    table.cell(fill: warm-fill)[内存中的目录项、元数据和文件数据。],
    table.cell(fill: warm-fill)[提供普通文件、目录和共享内存对象。],
    table.cell(fill: soft-fill)[devtmpfs 设备文件系统],
    table.cell(fill: soft-fill)[第三章设备能力注册表的投影事件和设备节点规范（`DevNodeSpec`）。],
    table.cell(fill: soft-fill)[把类型化设备能力落实为可打开的设备节点。],
    table.cell(fill: handoff-fill)[procfs 进程文件系统],
    table.cell(fill: handoff-fill)[任务表、内存统计、挂载状态和内核诊断信息。],
    table.cell(fill: handoff-fill)[读取时动态生成内容，目录项可随状态变化。],
    table.cell(fill: stable-fill)[pipefs 管道文件系统],
    table.cell(fill: stable-fill)[管道对象的环形缓冲区和读写端引用计数。],
    table.cell(fill: stable-fill)[不依赖用户可见挂载点，但使用统一文件对象模型。],
  ),
  kind: "表",
  continuation-kind: "续表",
  align: (left, left, left),
)

== 4.10 VFS 上下文与跨子系统交接

VFS 上下文（`VfsContext`）是进程可见文件系统状态的集合。它保存当前目录、进程根目录、挂载命名空间、凭据、umask 和资源限制。第五章中的 `clone` 根据标志决定共享或复制它。`CLONE_FS` 共享当前目录和根目录，普通 `fork` 则复制上下文。凭据更新时，调度层会同步替换 VFS 上下文中的凭据快照，使权限检查消费稳定对象。

VFS 与内存管理的交接主要发生在 `mmap` 和页故障。`mmap` 把文件对象转化为进程地址空间中的 VMA。页故障到来时，内存管理根据 VMA 找回文件和偏移，再请求文件系统提供对应内容。VFS 不直接操作用户页表，内存管理也不直接修改文件系统命名空间。二者通过文件对象和页故障回调交接。

VFS 与设备模型的交接发生在 devtmpfs 设备文件系统和设备文件投影器。设备模型发布设备能力。投影层生成设备节点规范。devtmpfs 设备文件系统创建节点并在打开时构造文件操作集。这个过程保持单向。设备移除时，设备能力先进入失效状态。已打开文件的后续 I/O 会在适配器中看到设备不可用。新打开则会被节点状态或适配器拒绝。这样可以保证热插拔时用户态名字空间和底层设备生命周期不会互相撕裂。

== 4.11 工程设计总结

虚拟文件系统子系统的设计集中处理了三个长期存在的工程矛盾。第一个矛盾是路径名的灵活性与对象身份的稳定性。路径可以移动、重命名和被多个硬链接引用，但已打开文件必须继续指向同一个对象。第二个矛盾是统一 ABI 与资源异构性。用户态希望所有资源都能通过文件描述符访问，内核内部却需要保留普通文件、设备、管道和动态状态文件的差异。第三个矛盾是缓存性能与一致性。目录项和索引节点缓存必须加速热路径，同时不能让 `rename`、`unlink`、`mount` 和设备移除留下错误结论。

虚拟文件系统子系统具备以下创新。

第一是把命名关系、对象身份和打开能力拆成三个层次。这个拆分来源于本内核的并发和生命周期问题，并在传统 VFS 概念之上重新校准了边界。目录项只描述父目录下的名称关系，因此可以安全缓存正向和负向查找。索引节点只描述文件对象本体，因此可以在多个目录项和多个文件对象之间共享。文件对象则描述一次已经完成授权的打开能力，因此可以在路径变化后继续稳定工作。早期若把路径和文件对象直接绑定，`rename` 与 `unlink-but-open` 的语义会变得难以实现。若把所有状态都放进索引节点，描述符级标志和共享偏移又会污染对象本体。当前分层使每个对象只承担一种生命周期。命名空间变化只影响目录项。对象回收只影响索引节点。读写能力只影响文件对象。这个边界让复杂 POSIX 语义可以通过组合得到，无需在每个系统调用里重新判断特殊情形。

第二是缓存和回收围绕引用关系设计，而不是围绕全局扫描设计。目录项缓存使用分片降低锁竞争。索引节点缓存使用弱引用避免把历史对象永久保活。挂载对象的繁忙检测通过打开计数完成，不扫描所有任务的文件描述符表。`unlink` 后的索引节点先从命名空间消失，再等最后一个强引用释放后调用驱动的驱逐操作。这个设计把大范围遍历转化为局部引用变化，热路径开销更可控。事后回顾，这种引用关系也让 `open`、`close`、`rename` 和 `unlink` 等高频组合路径更容易分析，因为每个对象的所有权来源是清楚的。

VFS 向上可以提供稳定的 POSIX 兼容层，向下可以容纳不同文件系统和设备投影，向侧面与进程、内存和设备管理保持清晰交接。路径解析、打开能力、挂载命名空间和对象回收分别处在各自边界内。新增文件系统时，只需要实现驱动接口。新增设备节点时，只需要补充投影器和适配器。新增进程隔离语义时，可以从 VFS 上下文和挂载命名空间扩展。VFS 因此成为用户态观察内核资源的结构化边界与整个操作系统的 POSIX 兼容层，而不仅仅是一个虚拟“文件系统”。
