# QEMU 开发测试环境

本目录仅负责测试环境。DiskStudio 只构建一份正式功能程序，本地打开与客体验收使用相同的 `diskstudio.exe` 和 SHA-256；应用代码没有 VM 身份白名单或专用构建分支。

所有 QEMU、下载、系统盘、测试盘、检查点、工具光盘、测试临时文件和日志集中在项目 `.vm/`。脚本不安装系统服务、不修改 PATH、不提权、不自动启用 Windows 功能。

## Btrfs 原生界面验收

`build-btrfs.ps1` 将 4Kn 客体工具绑定到已完成的桌面程序哈希。`Btrfs-Lab-4Kn-Create.exe` 核对身份后仅在 A 的未分配空间创建一个 512 MiB Linux 分区，并记录其分区号、位置和其余 GPT 布局。在同一工具光盘的正式程序内以 `DSBTRFS4KN` 为卷标格式化该分区。`Inspect` 复核范围、卷标、超级块与设备树，并确认 A 保持脱机和其余布局不变。`Online` 核对已格式化或已缩小的指定分区后联机 A，供原生界面依次调整到 256 MiB 与 768 MiB。每次调整后再次运行 `Inspect`。

Windows 正常关机后，`node scripts/vm/ubuntu.mjs btrfs-verify` 将同一对 4Kn 磁盘以只读方式接入 Ubuntu。提前用 `seed-btrfs-verify` 准备只读种子。Ubuntu 核对 A 上的卷标、分区类型和 768 MiB 容量，运行 `btrfs check --readonly`，并只读挂载核对空目录。正式构建 `28ac3b7b…` 在 A/33 的三项原生操作及 Ubuntu 独立检查均通过。记录见 `.vm/btrfs-batch-status.json`。

尾部条带搬移使用 `build-btrfs.ps1 -Relocation`，将同一桌面构建、两个来宾入口和 384 MiB 的 `DSBTRFSMOVE` 样本绑定到 SHA-256。用 `make-tools-iso.ps1 -Live -BtrfsMoveFixture` 选择 `Btrfs-Lab-4Kn-Move-Prepare.exe` 和 `Btrfs-Lab-4Kn-Move-Inspect.exe` 生成只读工具光盘。准备入口只在 A 的新 Linux GPT 分区写入样本，完成校验后联机 A。用桌面程序预览该分区缩到 192 MiB，预计搬移 80 MiB，再提交一次。检查入口核对 A 脱机、其余分区和来宾 OS 布局，并完整回读样本的 51 个文件。

如果首轮因空闲范围不足在建分区前结束，可用 `-Relocation -ReuseFixture` 和 `Btrfs-Lab-4Kn-Move-Reuse.exe`、`Btrfs-Lab-4Kn-Move-Reuse-Inspect.exe`。此入口按旧批次记录和固定字节范围核对已验收的 768 MiB 空白 A/33，锁卷脱机后再次验证其空目录，才准备同大小的有文件样本。原生缩容预览为 768→192 MiB，搬移量仍为 80 MiB。

Windows 正常关机后运行 Ubuntu 的 `btrfs-move-verify`。提前用 `seed-btrfs-move-verify` 建立只读种子。Ubuntu 只读检查文件系统和数据校验和，再挂载读出文件清单与内容 SHA-256。将该摘要与 Windows 后验记录核对。

正式构建 `0b731c98…` 有 752 项集中检查通过，1 项按配置跳过。本地、留档和客体载荷 SHA-256 一致。首轮准备工具在创建分区前受自身操作锁阻断，随后复用已验证的空白 A/33。原生预览和实际缩容均通过。A 保持脱机，B 写入计数在缩容中未增加。Windows 后验和 Ubuntu 只读核对的 51 个文件内容摘要一致。详见 `.vm/btrfs-relocation-batch-status.json`。

快照恢复样本由 `node scripts/vm/ubuntu.mjs seed-btrfs-snapshot-image` 和 `btrfs-snapshot-image` 生成。此阶段只给 Ubuntu 接入系统盘与种子光盘，文件系统建立在内存普通文件中。Linux 创建只读快照后删除一个内联文件和一个压缩文件，并保留改名、仍存在硬链接、未修改的三个对照。只读检查通过后，客体通过串口记录镜像及样本的 SHA-256。用 `extract-btrfs-snapshot.py` 读取该次串口日志，验证传输和解压，再保存普通镜像到 `.vm`。

用 `build-btrfs.ps1 -Snapshot` 绑定正式程序和镜像摘要，使用 `make-tools-iso.ps1 -Live -BtrfsSnapshotFixture` 选择 `Btrfs-Lab-4Kn-Snapshot-Prepare.exe` 与 `Btrfs-Lab-4Kn-Snapshot-Inspect.exe`。来宾先核实普通镜像恰好提供两个已删除候选，再在 A 的新 Linux GPT 分区写入同一镜像并回读，完成后 A 脱机。用原生程序「找回删除文件」扫描指定分区，保留目录导出两个候选到 B 的指定文件夹。检查入口复核输出内容、嵌套目录、导出清单、A 的源镜像和两盘 GPT。

真实快照样本检出了压缩文件末尾部分扇区的解码问题。修复后，`40e54920…` 的 753 项集中检查通过，1 项按配置跳过。同一正式程序在 4Kn A/34 上完成扫描、预览和导出，B 上的两个文件及嵌套目录经独立复查通过，两份摘要均与 Linux 原样本相符。A 保持脱机，源范围字节、两盘 GPT 及客体系统盘布局与基线一致。见 `.vm/btrfs-snapshot-batch-status.json`。

保留事务恢复样本使用 `node scripts/vm/ubuntu.mjs seed-btrfs-retained-image` 和 `btrfs-retained-image` 生成。Linux 同步后记录活动子卷的根地址和事务代次，随后删除文件，确认无快照并运行 `btrfs check --readonly --check-data-csum`。用 `btrfs restore -t` 指定旧根，在内存中独立复核两个文件。`extract-btrfs-snapshot.py` 同样可提取 `btrfs-retained-image-*.log` 中的镜像，校验串口传输、镜像摘要和样本清单。

用 `build-btrfs.ps1 -Retained` 绑定正式程序与镜像，使用 `make-tools-iso.ps1 -Live -BtrfsRetainedFixture` 选择 `Btrfs-Lab-4Kn-Retained-Prepare.exe` 和 `Btrfs-Lab-4Kn-Retained-Inspect.exe`。准备入口验证普通镜像中只有两个保留事务候选，再写入 A 的专用 Linux GPT 分区并回读。原生程序从脱机 A 扫描指定分区并按原目录导出至 B 上由准备入口报告的文件夹。检查入口复核完整内容、嵌套目录、导出清单、A 的源镜像和两盘 GPT。

正式构建 `40e54920…` 在 4Kn A/35 完成无快照样本扫描、预览和导出。Linux 从第 7 代根读出的两个文件与 B 上回读的内容和 SHA-256 相符，改名、仍有硬链接和未改动的文件均未列入候选。A 保持脱机，扫描、导出及后验期间未新增写入，源范围、两盘 GPT 和客体系统盘布局也与基线相符。导出清单记录两个文件和两个目录的时间或属性提示。见 `.vm/btrfs-retained-batch-status.json`。

使用 `node scripts/vm/ubuntu.mjs seed-btrfs-checksum-images` 和 `btrfs-checksum-images` 生成 xxHash64、SHA-256 和 BLAKE2b-256 普通镜像。该阶段只接入 Ubuntu 系统盘和只读种子，镜像在内存中创建。每个镜像含内联文本和嵌套目录中的 Zstd 文件。Linux 对每个镜像运行文件系统及数据校验检查，并重新只读挂载核对文件内容。用 `extract-btrfs-snapshot.py` 提取该次串口日志中的三份镜像与清单，存放在 `.vm/btrfs-native-checksum`。

用 `build-btrfs.ps1 -Sha256` 将 SHA-256 样本和正式构建绑定。用 `make-tools-iso.ps1 -Live -BtrfsSha256Fixture` 选择 `Btrfs-Lab-4Kn-Sha256-Prepare.exe` 和 `Btrfs-Lab-4Kn-Sha256-Inspect.exe`。在 4Kn Windows 来宾中准备 A 上的 256 MiB 样本，通过正式程序预览并缩容至 192 MiB。检查入口复核两个文件、A 的 GPT 末端变更、A 脱机状态、B 的 GPT 和来宾系统盘布局。Windows 关机后，用 `seed-btrfs-sha256-verify` 准备种子，再运行 `btrfs-sha256-verify`，在 Ubuntu 中只读核对缩容后的文件系统和数据。

三种普通镜像均完成 256→320→256 MiB 扩缩容往返及 256→192 MiB 缩容，两个文件的 SHA-256 一致。正式构建 `b5edec6d…` 在 4Kn A/36 通过 SHA-256 缩容、原生扫描和压缩文件预览。独立检查核对了 A 的 GPT 变更和脱机状态，B 的写入计数未增加。随后 Ubuntu 只读复查两份文件，内容与初始样本一致。见 `.vm/btrfs-checksum-batch-status.json`。

用 `node scripts/vm/ubuntu.mjs seed-btrfs-members-images` 和 `btrfs-members-images` 在隔离 Ubuntu 内生成两个普通成员镜像。样本以 RAID0 存放文件数据，以 RAID1 镜像元数据。Linux 复核文件系统及数据校验，并从只读挂载中读取跨条带文件及只读快照里的删除文件。以 `extract-btrfs-snapshot.py` 提取该次记录的镜像和摘要。

用 `build-btrfs.ps1 -Members` 以及 `make-tools-iso.ps1 -Live -BtrfsMembersFixture`，选择 `Btrfs-Lab-4Kn-Members-Prepare.exe` 和 `Btrfs-Lab-4Kn-Members-Inspect.exe`。准备入口验证普通镜像后，在 A 和 B 各创建一个 256 MiB Linux 分区并写入对应成员。两盘脱机后在原生界面选择报告中的两个分区，检查目录浏览、预览、删除候选和缺失成员的处理。检查入口独立回读两份成员镜像和两个文件，复核两盘 GPT 和客体系统盘布局。

用 `seed-btrfs-parity-images` 和 `btrfs-parity-images` 生成 2 设备 RAID5 和 4 设备 RAID6 样本。两种样本的数据、元数据及系统块组都使用对应的 RAID 配置。Ubuntu 只挂载其虚拟系统盘和只读种子光盘，在内存中的普通文件上生成样本并核对数据。用 `extract-btrfs-snapshot.py` 校验并提取镜像。

用 `build-btrfs.ps1 -Parity` 和 `make-tools-iso.ps1 -Live -BtrfsParityFixture` 选择 `Btrfs-Lab-4Kn-Parity-Prepare.exe` 和 `Btrfs-Lab-4Kn-Parity-Inspect.exe`。准备入口在 A 新建三个专用分区，写入一个 RAID5 成员与两个 RAID6 成员。A 脱机后，在原生程序显式允许缺盘读取。分别检查缺一个与缺两个成员时的目录和预览，再将 RAID6 的 `live/nested/striped.bin` 保留目录导出到报告指定的 B 盘目录。检查入口独立核对文件、三个成员镜像、两盘 GPT 与来宾系统盘布局。

用 `seed-btrfs-allocation-image` 和 `btrfs-allocation-image` 生成带独立块组树的单设备普通镜像。Ubuntu 核对分配和数据校验，并只读挂载验证共享范围、稀疏文件、预分配文件和快照内容。用 `extract-btrfs-snapshot.py` 提取镜像。用 `build-btrfs.ps1 -Allocation` 和 `make-tools-iso.ps1 -Live -BtrfsAllocationFixture` 选择 `Btrfs-Lab-4Kn-Allocation-Prepare.exe`、`Btrfs-Lab-4Kn-Allocation-Archive.exe`、`Btrfs-Lab-4Kn-Allocation-Inspect.exe`。准备工具报告 A 的源分区、B 的目标分区及备份路径。先用正式界面发起已用空间备份，再用 Archive 核验备份。之后执行已用空间克隆并运行 Inspect。后验核对全部已复制范围、B 原有的空闲区、五个文件以及两盘 GPT 和来宾系统盘布局。

用 `build-btrfs.ps1 -LostPartition` 和 `make-tools-iso.ps1 -Live -BtrfsLostFixture` 选择 `Btrfs-Lab-4Kn-Lost-Prepare.exe` 与 `Btrfs-Lab-4Kn-Lost-Inspect.exe`。准备工具核对 Linux 样本和完整 GPT，只在 A 的 14 GiB 起始、768 MiB 未登记且逐字节为零的范围放入两个样本。界面搜索报告指定的 LBA 范围，并从仅剩备用超级块的候选保留路径导出两个文件到 B。再选择主超级块完好的候选恢复一项 GPT Linux 数据条目。检查工具核对文件、完整源样本、前后 GPT 和来宾系统盘布局。

用 `build-btrfs.ps1 -LostMembers` 和 `make-tools-iso.ps1 -Live -BtrfsLostMembersFixture` 选择 `Btrfs-Lab-4Kn-Lost-Group-Prepare.exe` 与 `Btrfs-Lab-4Kn-Lost-Group-Inspect.exe`。准备工具仅在 A 的 12 GiB 起始、768 MiB 未登记且逐字节为零的范围放入两个 Linux RAID0 数据成员。按报告分别搜索两个成员范围，在第二次搜索中联合选取上一次搜索的成员，预览并保留目录导出 `striped.bin` 到 B。检查工具比较两个完整成员、原有 GPT、来宾系统盘布局与 Linux 文件 SHA-256。

用 `node scripts/vm/ubuntu.mjs seed-lvm-images-4kn` 和 `node scripts/vm/ubuntu.mjs lvm-images-4kn` 在单独的 Ubuntu 来宾生成普通 LVM2 镜像。Ubuntu 核对设备映射，分别检查 Ext4 线性卷和条带卷并读回嵌套文件。用 `extract-lvm-fixtures.py` 从验收串口日志提取两个镜像，随后使用 `build-lvm.ps1` 和 `make-tools-iso.ps1 -Live -LvmFixture` 选择 `Lvm-Lab-4Kn-Prepare.exe` 与 `Lvm-Lab-4Kn-Inspect.exe`。准备工具验证 A 两个固定范围未分配且逐字节为空后建立普通 Linux GPT 条目。按报告在文件恢复中先读取第一个物理卷，再读取完整卷组并保留目录导出条带文件到 B。后验核对两个逻辑卷、两份物理卷完整摘要、B 的导出及受保护布局。

镜像浏览验收先运行 `node scripts/vm/build-lvm-image.mjs`，将两个物理卷放入 GPT 普通文件镜像，再转换为动态 VHDX，使用 QEMU 比对逻辑字节。然后运行 `build-lvm.ps1 -Images`，制作只读光盘并选择 `Lvm-Images-4Kn-Prepare.exe` 与 `Lvm-Images-4Kn-Inspect.exe`。准备工具核对正式载荷，在 4Kn 测试 A 的 12 GiB 处校验空闲范围并创建 1 GiB NTFS 夹具卷，核对其他既有分区，再写入两个完整 RAW 和 VHDX。原生界面从「备份与克隆 → 浏览镜像」分别读取两种来源，预览线性卷并保留目录导出条带卷文件到 B 的两个输出目录。后验工具比对三个源文件完整摘要、两份导出和各自清单记录的镜像范围，以及准备后的 A/B GPT 和来宾 OS 布局。

RAID 镜像验收使用 Ubuntu 的 `seed-lvm-raid-matrix-4kn` 和 `lvm-raid-matrix-4kn` 阶段生成五个物理卷。Linux 等待阵列同步，核对七个 Ext4 逻辑卷并独立读回各自的文件。用 `extract-lvm-fixtures.py` 提取带摘要的镜像，再运行 `node scripts/vm/build-lvm-image.mjs --raid` 将五个物理卷组装成 VHDX。运行 `build-lvm.ps1 -Raid`，并用 `make-tools-iso.ps1 -Live -LvmFixture` 选择 `Lvm-Raid-4Kn-Prepare.exe` 和 `Lvm-Raid-4Kn-Inspect.exe`。准备工具将 VHDX 写入 A 上已有的 1 GiB NTFS 夹具卷。按工具提示在正式界面选择全部五个成员，将 `r1` 和 `r6` 的 `nested` 文件夹导出到 B 上各自的目录。后验工具核对 Linux 文件摘要、五个来源范围、源 VHDX 摘要、两盘 GPT 及来宾系统盘布局。

缺盘读取验收复用同一份 RAID VHDX。运行 `build-lvm.ps1 -RaidDegraded`，并在只读工具光盘选择 `Lvm-Degraded-4Kn-Prepare.exe` 和 `Lvm-Degraded-4Kn-Inspect.exe`。准备工具核对 A 上已存在的镜像，在 B 创建本次输出目录。原生界面先选 PV 1、PV 2 和 PV 4，验证默认状态下需要缺失成员的卷不可读。勾选允许缺盘读取后，将缺一个成员的 `r5` 和缺两个成员的 `r6` 的 `nested` 分别导出到 B。后验核对 Linux 摘要、缺盘清单标记、三个所选成员范围和分区布局。

RAID10 far 和 offset 验收使用 Ubuntu 的 `seed-lvm-raid10-layouts-4kn` 和 `lvm-raid10-layouts-4kn` 阶段。LVM2 分配夹具的组件，Linux dm-raid 分别写入全组轮转的 far 和组内轮转的 offset。普通文件夹具的 LVM2 文本记录对应的布局和位图区域。用 `extract-lvm-fixtures.py` 提取普通文件后，运行 `node scripts/vm/build-lvm-image.mjs --raid10` 和 `build-lvm.ps1 -Raid10`，在只读工具光盘选择 `Lvm-Raid10-4Kn-Prepare.exe` 与 `Lvm-Raid10-4Kn-Inspect.exe`。在原生界面选齐四个成员，分别导出两个卷的 `nested` 目录。后验核对 Linux 文件摘要、四个来源范围、完整镜像及受保护布局。

跨盘删除恢复验收使用 Ubuntu 的 `seed-lvm-cross-deleted-4kn` 和 `lvm-cross-deleted-4kn` 阶段生成两个普通物理卷文件。384 MiB 条带逻辑卷内的 Btrfs 只读快照保留了已从源子卷删除的 `nested/deleted.bin`。Linux 独立检查文件系统和数据校验，并以只读挂载确认删除文件、改名对照和现存对照。用 `extract-lvm-fixtures.py` 提取夹具后，运行 `build-lvm.ps1 -CrossDeleted`，在只读工具光盘选择 `Lvm-Cross-4Kn-Prepare.exe` 和 `Lvm-Cross-4Kn-Inspect.exe`。准备工具核对正式载荷，只在 A 的 13、13.5 GiB 和 B 的 13 GiB 处逐字节空白且未分配的范围创建分区。先选 A 与 B 的成员，扫描唯一删除候选并验证 B 不能作为输出。再选 A 的两个成员，将同一候选保留目录导出到 B。后验核对原始 SHA-256、历史目录、物理卷导出清单和受保护的布局。

薄置备镜像验收使用 Ubuntu 的 `seed-lvm-thin-snapshots-4kn` 和 `lvm-thin-snapshots-4kn` 阶段。薄池的数据和元数据分别放在两个物理卷中。Linux 从 Ext4 原卷生成快照并改写原卷，再独立核对两个版本的文件与未分配空间。用 `extract-lvm-fixtures.py` 提取普通文件，运行 `node scripts/vm/build-lvm-image.mjs --thin`、`build-lvm.ps1 -Thin`。用 `make-tools-iso.ps1 -Live -LvmFixture` 选择 `Lvm-Thin-4Kn-Prepare.exe` 和 `Lvm-Thin-4Kn-Inspect.exe`，复用 A 上已有的夹具卷。从镜像浏览入口分别选择 `origin` 和 `point1`，将各自的 `nested` 文件夹导出到报告指定的 B 盘目录，再运行后验工具核对两个版本和受保护的分区布局。

传统快照验收使用 Ubuntu 的 `seed-lvm-cow-persistent-4kn` 和 `lvm-cow-persistent-4kn` 阶段。Linux 在不同物理卷上放置原卷和 COW 存储，创建两代快照并改写原卷。在 Ext4 文件系统之外额外变更 2 MiB，覆盖多个例外表区域。另创建以零卷为原卷的稀疏快照。用 `extract-lvm-fixtures.py` 提取镜像，运行 `node scripts/vm/build-lvm-image.mjs --cow` 和 `build-lvm.ps1 -Cow`。用 `make-tools-iso.ps1 -Live -LvmFixture` 选择 `Lvm-Cow-4Kn-Prepare.exe` 和 `Lvm-Cow-4Kn-Inspect.exe`，复用 A 上已有的夹具卷。从正式界面分别导出 `point1` 和 `sparse` 的 `nested` 到 B，随后核对两份文件、镜像清单和分区布局。

正式构建 `16319c7c…` 核对了 A/37 + B/4 的完整成员组。原生界面拒绝单成员扫描，选齐后找到唯一的快照删除候选，并通过文本预览。磁盘管理的浏览入口列出并预览 640 KiB 的跨条带文件。独立回读确认两个文件与 Linux 摘要一致，两份完整成员镜像、GPT 和客体系统盘布局也与基线相符。两盘保持脱机，扫描和后验期间的写入计数均未增加。两盘都用作源成员，本用例覆盖原生读取和恢复候选，目标盘导出另行验收。见 `.vm/btrfs-members-batch-status.json`。

## 扇区范围复制

`build-sector-copy.ps1` 绑定已完成的桌面构建，只编译 4Kn 客体核对工具。将 `Sector-Copy-4Kn-Baseline.exe`、`Sector-Copy-4Kn-Verify.exe` 和 `Sector-Copy-4Kn-Verify-Report.exe` 与同一份 `DiskStudio.exe` 放入只读工具光盘。验证 payload SHA-256，运行 `node scripts/vm/lab.mjs preflight`，再在 DiskStudio-Test 中启动基线工具。

镜像扇区复制使用 `build-sector-copy.ps1 -Image` 和 `start --4kn`。如 A 的 DSGLEFTKEEP 卷还没有盘符，先核对 A 的序列号和该 NTFS 卷身份，再在来宾中分配空闲盘符。Prepare 核对来宾身份和布局，在该卷建立 4Kn GPT 镜像，并核对 B/7 的测试范围全零。在「备份与克隆 → 浏览镜像」打开 Prepare 报告的文件，选择 4096 B 整盘镜像。打开「复制镜像扇区」，选择镜像中的分区，从相对 LBA 4 反向复制 8 个扇区到 B 的新文件。File 检查原镜像和导出字节。将同一来源范围复制至 B/7 相对 LBA 4608，核对预览后确认。Device 检查目标字节、脱机状态以及 GPT、客体系统盘布局和目标区外数据。三个工具报出的目标区外 SHA-256 应一致。

损坏 NTFS 验收使用 `build-guest-checks.ps1 -NtfsReconstruction` 和默认的 512 字节 `start` 配置。把 `Ntfs-Reconstruct-Prepare.exe`、`Ntfs-Reconstruct-Inspect.exe`、`Ntfs-Reconstruct-Seed.exe`、`Ntfs-Reconstruct-Verify.exe` 与同一正式构建放入只读光盘，校验 SHA-256 后进行预检。Prepare 仅在 B 的固定全零空闲范围建立一次性 2 MiB 夹具，使主引导和 MFT 记录 0、1 损坏，并让 B 保持脱机。它在 A/2 建立 `DS-NTFS-Reconstruction` 目录和同内容镜像。若 Prepare 在创建分区后、写入夹具前停下，先运行只读的 Inspect 核对两盘 GPT、原生布局和目标范围是否全零。仅在 Inspect 全部通过后才使用 Seed 填充这个已创建的分区。

使用「文件恢复 → 重建文件视图」选择工具报告的 B 分区，预览 `file-with-12345`，以平面布局导出到 A 的夹具目录。Verify 核对文件、清单中的重建依据、全部源字节、相邻扇区、两盘 GPT 和客体系统盘布局。镜像入口以 512 字节扇区的分区镜像打开 `damaged-ntfs.img`。备份入口从该 B 分区在 A 的同一目录创建并验证完整备份，再浏览备份。两个入口均用「重建文件视图」核对同名文件的预览字节 `31 32 33 34 35`。

FAT32 和 exFAT 备用引导验收使用 `build-guest-checks.ps1 -FatReconstruction` 和默认 512 字节配置。只读工具光盘包含匹配的正式程序、`Fat-Reconstruct-Prepare.exe` 和 `Fat-Reconstruct-Verify.exe`。Prepare 在已验证的 A/2 卷上新建 `DS-FAT-Reconstruction` 和三个普通镜像文件。以 512 字节分区镜像分别打开 `damaged-fat32.img` 和 `damaged-exfat.img`，在「重建文件视图」预览 `FATVIEW.TXT` 和 `exfat-view.txt` 并核对文件字节。打开 `conflicting-exfat.img` 验证主备身份冲突会停止重建。Verify 独立比较三个完整源文件、两盘 GPT、客体系统盘布局及 B 的在线和只读状态。

丢失分区的备用引导验收使用 `build-guest-checks.ps1 -LostBackupBoot` 和默认 512 字节配置。只读光盘包含同一正式程序、`Lost-Backup-Boot-Prepare.exe` 和 `Lost-Backup-Boot-Verify.exe`。Prepare 核对两盘 GPT、B 的原生布局和准确的全零空闲区，再锁定卷和脱机 B。仅在 13.5 GiB 起点的专用 48 MiB 范围写入三个未登记的损坏卷，A/2 上的 `DS-LostBackupBoot` 保存基线和导出文件。在正式程序中选 B，按 Prepare 报告的 LBA 和数量搜索。分别打开 NTFS、FAT32 和 exFAT 候选，并核对「仅浏览与提取」及预览。将 `FATVIEW.TXT` 和 `exfat-view.txt` 各导出一次至 A 的夹具目录。Verify 比较两个文件的字节、SHA-256 和备用引导清单，并复核 B 的完整夹具、填充区、相邻扇区、脱机状态、两盘 GPT 及来宾系统盘布局。

基线要求 A 脱机，B 有在线的 `DS4KMOVEJRN` 卷，并记录 A/B 的 GPT 和客体 OS 布局键。用光盘上的正式程序将 A 的整盘 LBA 0 起 2 扇区、起点为 8704 MiB 的 384 MiB 分区内前 3 扇区，以及整个 A 的当前扇区 0 分别导出到 B 的该卷，保留建议文件名。另从 B 选择同一卷作目标，确认拒绝。核对工具读取限定的 A 盘扇区，与 B 上文件逐字节比较和计算 SHA-256，复核 GPT、布局键、磁盘状态及未完成文件是否存在。Report 入口只读取已保存的核对结果。

如果 QEMU 窗口没收到宿主按键，先聚焦客体的扇区数量框，再用 `node scripts/vm/lab.mjs sector-copy-count-keys 2` 或 `3`。该命令重新预检 4Kn 客体，只发送指定的测试数量。

读取故障验收用 `build-sector-copy.ps1 -Fault`，将 `Sector-Copy-Fault-4Kn-Prepare.exe` 和 `Sector-Copy-Fault-4Kn-Verify.exe` 与匹配的正式程序放入只读光盘。`node scripts/vm/lab.mjs start --4kn --sector-copy-fault` 先暂停来宾并核对 Test A 的完整块设备链，再启动。在 A 的 LBA 2232323 注入单个读取故障。Prepare 核对来宾身份与布局，将 A 脱机，并独立读取故障及相邻扇区。用原生界面从整盘 LBA 2232320 复制八个扇区。默认停止应保留 `ds-sector-strict.bin.partial`，正向填零且额外跳过数为 0 时保存 `ds-sector-forward.bin`，反向选 `LOSS` 和额外跳过两个扇区时保存 `ds-sector-reverse.bin`。在 B 的同一报告指定卷上保存反向 JSON 为 `ds-sector-reverse.json`。Verify 逐字节核对两个导出、严格模式的未完成状态、独立故障地址、报告和 GPT。

文件续作和已填充复读用 `build-sector-copy.ps1 -Continuation`。在 4Kn 故障配置运行 `Sector-Continuation-4Kn-Prepare.exe`。以默认停止策略从 A 的整盘 LBA 2228227 正向导出 4104 扇区至 B 的 `ds-sector-resume.bin`。Interrupted 检查持久断点、16 MiB 未完成文件和故障。在同一故障启动下，从 A 的 LBA 2232320 反向导出 8 个扇区至 `ds-sector-recopy-base.bin`，配置 LOSS 和额外跳过 2 个扇区。选择「复读已填充扇区」，打开该文件的 `.dscopy`，保存 `ds-sector-recopy-result.bin` 和 `ds-sector-recopy-result.json`。Reread 核对原文件、新文件和报告。正常关闭来宾后使用 `start --4kn` 启动同一来宾，重新通过预检。在 A 的整盘范围打开 `ds-sector-resume.bin.dscopy`，完成后运行 Finish，核对完整文件与前缀。所有导出和断点均写入 B。直接写盘的持久断点需要源盘和目标盘之外的物理磁盘，本夹具覆盖文件续作和文件复读。

旧报告导入用 `build-sector-copy.ps1 -Legacy`，继续用 4Kn 故障配置。Prepare 核对旧版 `ds-sector-reverse.json`、原 `.bin` 和 B/7 相对 LBA 4096 的空白目标区，独立生成 v2 报告。文件模式选择「用旧 JSON 报告复读」，选原报告和 `ds-sector-reverse.bin`，保存 `ds-sector-legacy-file.bin`。File 独立比对新旧文件。磁盘模式先从 A 整盘 LBA 2232320 向 B/7 相对 LBA 4096 反向复制 8 扇区，配置 LOSS、额外跳过 2，关闭同次延后重读与断点保存。Rearm 核对脱机 B 的旧输出和保护布局之后才为第二次确认联机 B。在同一个源和目标下导入 `ds-sector-legacy-device.json`，复核绝对目标 LBA 3477760，再确认。Device 读取整个测试分区、相邻扇区及两盘 GPT，并核对 A/B 仍脱机。Prepare、Rearm、Device 报出的目标区外综合摘要须一致。

直写目标盘验收使用同一故障启动参数及 `build-sector-copy.ps1 -Device`，将 `Sector-Copy-Device-4Kn-Prepare.exe`、`Sector-Copy-Device-4Kn-Continue.exe`、`Sector-Copy-Device-4Kn-Verify.exe` 与同 SHA-256 的正式程序放入只读光盘。Prepare 在 B 的已校验全零空闲范围内创建一个 32 MiB 测试分区，并报告分区号、绝对起始位置和两次复制的参数。原生界面先检查 B 的整盘 LBA 0 预览被拒绝，再按 Prepare 报告复制 A 的八个健康扇区到 B 指定整盘位置。B 保持脱机时，Windows 可能不提供隐藏等保护属性，此时下一次预览必须拒绝。Continue 独立核对第一次复制后的完整 32 MiB、邻接扇区、两盘 GPT 和来宾系统盘布局，再将测试盘 B 联机以准备第二个用例。刷新软件的磁盘列表，把八个含故障的扇区反向复制到 B 的测试分区相对 LBA 40，填充文本 `LOSS`，额外跳过两个扇区。Verify 逐字节检查整个 32 MiB 和边界两侧，复核两盘保持脱机、GPT 和来宾系统盘布局一致。

延后重读用 `build-sector-copy.ps1 -Reread`，与同 SHA-256 的正式程序一起放入只读光盘，使用同一 4Kn 故障启动参数。Prepare 复用已验证的 B/7 测试分区，核对目标区全零和两个 GPT 后才为测试联机 B。先从 A 的 LBA 2232320 反向复制 8 扇区到 B 的普通文件 `ds-sector-reread-checked.bin`。选择 `LOSS`、额外跳过 2 扇区并启用重读，保存 `ds-sector-reread-checked.json`，运行 Verify-File。再从 A 的 LBA 2232064 正向复制 1024 扇区到 B/7 相对 LBA 2048，选择跳过大段后重读及 `LOSS`。预期 384 扇区延后，383 扇区在重读时恢复。Verify-Device 检查整个测试分区、相邻扇区、两盘 GPT、来宾系统盘布局及脱机状态。

## 坏道检测的逐扇区细查

若共用夹具仍保留其他验收的联机状态，先用无读故障的 `start --4kn` 启动并通过预检。以 `build-guest-checks.ps1 -SurfaceRefinementSetup` 制备 `Surface-Refinement-4Kn-Prepare.exe`。关闭正式程序后运行 Prepare。它核对 A/B 标识、保护状态、GPT 及 B 的保留输出分区，只将 A 脱机、B 联机，随后重新核对两盘 GPT 与客体系统盘布局。正常关机后再启动故障场景。

复用 `node scripts/vm/lab.mjs start --4kn --sector-copy-fault` 的固定读故障。用 `build-guest-checks.ps1 -SurfaceRefinement` 绑定当前正式构建，将 `Surface-Refinement-4Kn-Inspect.exe` 和 `Surface-Refinement-4Kn-Verify.exe` 放入只读工具光盘。来宾预检后运行 Inspect，确认脱机 A、联机 B 和报告输出卷。Inspect 对比故障扇区及其相邻扇区的读取结果。

在正式界面的「坏道检测」选 A 整盘，从 LBA 2232320 检测 512 个扇区。先关闭细查，将报告保存到 Inspect 指定 B 卷上的 `SurfaceCoarse.json`。再启用细查，在同一范围运行，将报告和进度保存为 `SurfaceRefined.json` 与 `SurfaceRefined.dsscan`。加载该进度并核对设置与结果。Verify 读取 B 上的三个文件，并对比两次检测的 1 MiB 初读失败范围、细查的 16 次追加读取、LBA 2232323 的单扇区读取错误和未定位计数。QMP 记录测试盘的读写计数。

正式构建 `e1c80fbf…` 通过上述 4Kn 场景。两份报告显示相同的初读失败块。细查确认一处 Win32 1117，另有 255 个扇区在细查时读通，未定位计数为 0。保存的进度成功恢复选项和结果，独立工具核对三个文件通过。QMP 记录显示两次扫描期间 A/B 无新增写入，进度加载期间 A 的读取计数也未变化。512 字节扇区和细查中断续扫由合成测试覆盖。证据见 `.vm/surface-refinement-status.json`。

## 坏道检测的单次读取超时

使用已准备的 4Kn A/B 夹具和匹配的正式构建。保持 A 脱机及 B 输出卷联机。`build-guest-checks.ps1 -SurfaceTimeout` 构建只读核对工具，用 `make-tools-iso.ps1 -Live -GuestTools @('Surface-Refinement-4Kn-Inspect.exe','Surface-Timeout-4Kn-Verify.exe')` 制备工具光盘。通过带固定故障的 `start --4kn --sector-copy-fault` 和预检后，在来宾运行 Inspect 核对 A/B 和正式程序摘要。在正式界面选择 A 整盘、起点 LBA 2232576、512 扇区、超时 1 秒。开始前执行 `node scripts/vm/lab.mjs surface-read-window`，它只给 A 设置读限速。超时界面须显示尚未完成的请求，失败块计数应为零。保存到 B 的 `SurfaceTimeout.dsscan`。

执行 `node scripts/vm/lab.mjs surface-read-clear` 后加载该进度。超时应恢复为 1 秒。改为 3 秒并继续，检查全部 2 MiB 正常读完。保存 `SurfaceTimeoutResumed.json` 到同一 B 卷。Verify 复核正式程序摘要、两份文件的来源和范围、超时请求及续扫设置。用 QMP 核对扫描或加载阶段的 A/B 读写计数。最后用同一正式程序复查带故障的 LBA 2232320 起 512 扇区，开启细查时应再次定位 LBA 2232323。

构建 `6c652c18…` 已通过这组 4Kn 测试。超时请求从绝对 LBA 2232704 开始，覆盖 128 扇区，界面记录了 0 字节已完成和零介质错误。进度加载期间 A 盘未发生读写，续扫读完 2 MiB。限速扫描、续扫和故障细查期间 A/B 写入计数均未增加。验收记录位于 `.vm/surface-timeout-status.json`。

## FAT 与 exFAT 保留数据缩放

`build-fat-resize.ps1` 只编译客体驱动，绑定已完成桌面程序的 SHA-256。只在通过身份、容量和隔离预检的 DiskStudio-Test 内运行 `Fat-Resize-Check.exe`。它用跨构建的单次标记保护八次操作，匹配 A 的四个已有测试卷并各建立独立文件，不删除或格式化分区。每次缩放后核对脱机状态、精确 GPT 几何、独立读取的 65,553 字节文件及 SHA-256。在后续扩容之前单独联机并确认 Windows 能识别缩容的卷。失败停止且不重试，结束时 A 保持脱机，B 只用于核对。

正式程序 `45c36059…` 的八项隔离设备验收全部通过。原生程序另在 B 上只预览并取消 QBFAT16 128→64 MiB，保持两盘写入计数不变。`Fat-Resize-Report.exe` 可只读查看保存的末尾事件。完整证据见 `.vm/fat-resize-batch-status.json`。

尾部簇搬移使用 `build-fat-resize.ps1 -Relocation` 生成独立入口。客体驱动绑定原有 A 盘夹具，通过 Windows 文件分配放置样本，并在执行前要求正式预览明确报告非零搬移簇数。使用 `-Remaining` 构建的续验入口先核对首轮同一桌面构建在 FAT12 样本创建阶段因空间不足停止的记录，再鉴别并测试 FAT16、FAT32、exFAT 三个尚未操作的卷。两个入口使用独立单次标记，各自的 `Report` 入口只读取保存日志。

正式构建 `27012e24…` 完成 662 项集中检查。续验的三种卷均通过缩容和回扩，独立核对 GPT、Windows 识别状态及 65,553 字节文件。实际搬移 17、65、17 个簇，A 结束时脱机，B 写入计数不变。原生界面在 B/17 使用 MiB 预览 128→127 MiB 并取消。FAT12 的尾部搬移有合成验证，首轮因准备失败未执行客体缩容。记录见 `.vm/fat-relocation-batch-status.json`。

分配表容量批次由 `build-fat-resize.ps1 -Capacity` 绑定正式程序，使用 A 盘未分配范围上的两个新夹具。测试独立读引导区几何、比较完整 GPT，并通过恢复引擎逐字节检查样本。FAT12 首轮扩容通过后在清理已创建的占位文件时遇到 Windows 路径错误。`-Capacity -Remaining` 核对这一记录并只读枚举 FAT12 文件，再独立验收尚未开始的 exFAT。`-Fat12Tail` 绑定同一正式程序和两段历史记录，验证两个样本、占位文件长度及其位于 A/20 的句柄，将占位文件长度置零以释放空间。随后执行先前未提交的 FAT12 缩容。各入口只派发一次，独立 `Report` 读取保存日志。

正式构建 `d6b64df6…` 的集中检查共 664 项通过。A/20 FAT12 1→2→1 MiB，扩容和缩容各搬移 129 簇。A/21 exFAT 96→1088→96 MiB，扩容搬移 21 簇，位图 1→9→1 簇。独立读回样本及 GPT 均通过。真实设备四次调整限 A，最终 A 脱机、B 写入计数不变。原生界面核实占位还在时缩容预览拒绝，取消后两盘写入计数不变。详见 `.vm/fat-capacity-batch-status.json`。

`build-fat-resize.ps1 -Sectors` 将两个扇区规格的验收驱动绑定到现有正式程序哈希。默认配置运行 `Fat-Sectors-512-Check.exe`。要切换到 4Kn，请先正常关闭客体，再用 `node scripts/vm/lab.mjs start --4kn` 启动同一台隔离测试 VM。在 `.vm/windows` 中单独使用 `test-a-4kn.qcow2` 和 `test-b-4kn.qcow2`，系统盘仍为原 80 GiB 镜像。QMP 核对控制器上的逻辑和物理扇区大小、序列号和文件后端，客体再核对 Windows 报告的扇区和完整序列号。通过 `preflight` 后运行 `Fat-Sectors-4Kn-Check.exe`。两个驱动都用独立单次标记，写入只限 A，4Kn 的 B 保持 RAW。以普通 `start` 再次启动即可使用原有两块 NVMe 镜像。

512 字节扇区的 FAT16（A/22）通过 64→160→64 MiB，FAT32（A/23）通过 320→448→320 MiB。两者都扩大了 FAT，客体独立读回 65,553 字节样本并核对 GPT，A 最终脱机，B 的 GPT 和写入计数一致。

首轮 4Kn 驱动完成 A/2 FAT16 的准备，卷未卸载干净时预览拒绝，未派发缩放。`-Sectors -SectorDiagnosis` 通过只读核对两份 FAT 的保留项和样本，确认当时卷已恢复干净。`-Sectors -Remaining` 生成 `Fat-Sectors-4Kn-Continue-Check.exe`，绑定正式程序与首轮停止记录，复用原 A/2 和文件，在客体测试中等待 Windows 清除卷状态后才请求首次缩放。各 `Report` 入口只读保存的记录。首轮及续验入口都只运行一次。

续验通过 FAT16 64→160→64 MiB，随后 FAT32 预览因主备引导扇区不一致拒绝。`-SectorTail` 生成只读 FAT32 诊断和仅测试后续两项的入口。诊断读出 A/3 两份 4 KiB 引导扇区逐字节一致，A/B 写入计数不变。后续两项通过 FAT12（A/4）8→12→8 MiB 和 exFAT（A/5）96→1088→96 MiB，exFAT 位图 1→9→1 簇。`-SectorFat32` 验证原停止记录、原样本和当前主备引导区，再执行 A/3 首次缩放。FAT32 320→448→320 MiB 通过，每份 FAT 从 80 增至 112 个扇区。四类 4Kn 试验均独立核对文件、GPT 和引导几何，A 最终脱机，B 保持 RAW 且全程零写入。参见 `.vm/fat-sectors-batch-status.json`。

## GPT 分区移动验收批次

`build-partition-move.ps1` 将客体驱动绑定到正式桌面 SHA-256。默认 512 字节配置通过 QMP `preflight` 后，客体中的 `Partition-Move-Prepare.exe` 核对 UUID、完整序列号和两盘容量。在 A 的空闲区准备 512 MiB FAT16 和固定样本，在 B 上确定已有 NTFS 卷为断点存储。它保存两盘 GPT 基线，并报告 A 的分区号、原起点、向右移动 64 MiB 的目标起点和 B 的盘符。准备入口只允许运行一次。

用光盘上的正式程序预览并确认上述范围，断点保存到驱动核对的 B 卷。要验收跨进程和重启续作，在复制阶段已有落盘进度后使用「停止并保留断点」。`Partition-Move-Interrupted.exe` 只读确认 A 脱机、原 GPT 完整、B GPT 一致、断点位于 B 且记录了非零进度。重启同一客体后，从磁盘菜单打开「继续分区移动」，选同一个文件并再次确认。`Partition-Move-Inspect.exe` 独立核对最终 GPT、FAT 引导区隐藏扇区计数及完整样本。若原生操作已完成，直接做最终检查。若任一步失败，保留原记录和当前盘面。

首轮正式构建 `b5ec7564…` 在原生界面完成 A/24 的重叠移动。独立核对 GPT、FAT16 引导区和 131,089 字节样本通过。重启后 A 保持脱机，原断点通过当前 Windows 用户的来源校验。`-Diagnose` 生成的只读驱动确认该构建复核时遇到空的分区脱机、隐藏、只读和卷影复制字段。诊断期间 A/B 写入计数不变。见 `.vm/partition-move-windows-status.json`。

`build-partition-move.ps1 -Stop` 为后续隔离验收生成 `Partition-Move-Stop-Prepare.exe`、`Partition-Move-Stop-Interrupted.exe` 和 `Partition-Move-Stop-Inspect.exe`。先用正式程序只读复核上一轮的原断点，再运行 `Stop-Prepare`，它只在 A 的空闲范围创建新的 DSMOVESTOP 夹具，B 上使用独立的 `DiskStudio-move-stop` 文件夹。`node scripts/vm/lab.mjs move-stop-window` 在完整 QMP 预检后将 A 写入限为 8 MiB/s，以便在原生界面复制阶段使用停止按钮。断点只读检查后用 `move-stop-clear` 解除限速，再正常关闭并重启客体，从相同断点继续。`Stop-Inspect` 核对最终布局和样本。

正式构建 `e47addd6…` 通过 679 项集中测试，原生只读复核首轮已完成的断点通过，期间 A/B 写入计数不变。A/25 的 FAT16 在 12894→12958 MiB 移动过程中按下停止，独立读取确认已记录 220 MiB 且原 GPT 完整。重启后 A 仍脱机，原生界面读取同一断点并续作。A 的写入量为剩余 292 MiB 加五个 512 字节的引导及 GPT 扇区。独立核对新位置的 FAT 引导、完整样本和 A/B 两盘 GPT 通过。记录见 `.vm/partition-move-resume-status.json`。

`build-partition-move.ps1 -FourKn` 使用已通过集中测试的同一正式程序生成 4Kn 对应的 `Exfat` 和 `Ntfs` 准备及只读检查入口。正常关闭客体后用 `node scripts/vm/lab.mjs start --4kn` 更换为独立的 4Kn A/B 镜像，完成 QMP 预检。`Partition-Move-4Kn-Exfat-Prepare.exe` 一次性初始化仍为 RAW 的 4Kn B，创建 256 MiB NTFS 断点卷。在 A 的空闲区创建 96 MiB exFAT 样本并计划向右移动 4 MiB。原生操作与只读后验完成之后再准备 128 MiB NTFS 样本，向左移动 4 MiB。各用例使用独立的断点目录。只读入口复核正、副引导区、两盘 GPT 及 131,089 字节样本，exFAT 另外复算两个引导区的校验和。

4Kn exFAT 原生移动和独立后验均通过。NTFS 准备阶段完成了 Windows 文件回读，随后原始文件检查拒绝。`-FourKnDiagnose` 绑定原准备报告、夹具范围和程序散列，只读复核原文件的 Windows 文件 ID、MFT 记录、分配表及原始文件内容。只有原始文件内容和分配表复核都通过时，才单独保存原生移动所需的测试基线。见 `.vm/partition-move-4kn-status.json`。

同一构建 `e47addd6…` 在 A/6 上完成 96 MiB exFAT 的 548→552 MiB 重叠移动。NTFS 原夹具的只读复查通过，A/B 写入计数不变。随后在 A/7 上完成 128 MiB NTFS 的 796→792 MiB 重叠移动。两项都验证了全部样本、主备卷引导区和两盘 GPT，A 完成时脱机，独立检查期间 A/B 写入计数均不变。

`build-partition-move.ps1 -FourKnFat` 复用这份正式程序和已有 FAT 样本。`Partition-Move-4Kn-Fat12-Prepare.exe` 在检查 NTFS 的已完成断点和两盘 GPT 后联机 A，按 664 MiB 起点、8 MiB 长度和 `DSSEC12` 标签识别 FAT12，核对原有 65,553 字节样本，给出向右移动 4 MiB 的预览参数。原生移动后用对应的 `Inspect` 复核。随后 `Partition-Move-4Kn-Fat32-Prepare.exe` 核对 FAT12 已完成，再选择 212 MiB 起点、320 MiB 长度的 `DSSEC32` FAT32 向左移动 4 MiB。预览使用 Windows 当前分区编号。两个用例在 `DS4KMOVEJRN` 卷使用独立目录。4Kn 下 `move-stop-window` 仅将 A 的写入速度限制为 4 MiB/s，供 FAT32 复制阶段停止。用对应的 `Interrupted` 检查原 GPT 和非零断点进度，解除限速并正常重启，再从同一断点继续。最终由 `Inspect` 独立读回样本、主备 FAT32 引导区和两盘 GPT。

如旧版 FAT12 准备工具仅联机了 A，随后因分区重编号退出，`-FourKnFatContinue` 生成 `Partition-Move-4Kn-Fat12-Continue-Prepare.exe` 及按固定范围定位的 `Partition-Move-4Kn-Fat32-Range-Prepare.exe`。FAT12 准备续作要求 A 已联机、旧的一次性标记存在、两盘 GPT 通过上一用例的证明，且当前用例的基线文件尚未建立。

## NTFS 分区合并验收批次

集中测试和桌面构建完成后，用 `build-partition-merge.ps1` 绑定正式程序的 SHA-256 并生成四个客体入口。用 `start --4kn` 和 `preflight` 核对隔离配置，刷新只读工具盘。所有入口仍在客体核对 UUID、盘容量和完整序列号。`Partition-Merge-4Kn-Prepare.exe` 核对光盘上的桌面哈希，只在 A 的空闲区新建相邻的 256 MiB `DSMERGEKEEP` 和 128 MiB `DSMERGEFROM`，准备确定内容的文件、命名流、硬链接和 Unicode 文件名，并保存两盘 GPT 基线。

准备通过后，可用同一光盘的桌面程序预览并取消，从 `DSMERGEKEEP` 打开「合并分区」，选择右侧 `DSMERGEFROM`，核对先迁移和读回文件、再删除及扩容的顺序。`Partition-Merge-4Kn-Check.exe` 使用同一生产后端和同一对 GPT 身份，只派发一次实际合并。它独立比较最终 384 MiB 布局、B 的完整 GPT、保留卷的原文件、迁移内容与命名流、硬链接、Windows 所有者/主组/DACL 和修改时间。`Inspect` 独立读回布局和样本，`Report` 读取保存的执行日志。准备或合并失败时保留现场和日志，先核对已经提交的步骤。

首轮正式程序 `d39743b2…` 在原生界面通过 A/9 合入 A/8 的预览及取消，A/B 写入计数保持一致。合并驱动在建立源清单时退出。`-Diagnose` 用同一正式程序生成只读诊断入口，核对两盘 GPT、源样本以及目标目录尚未创建，再读取 Windows TxF 保留文件的名称和属性。`-Continue` 绑定了修正后的正式程序 `28139773…` 和原夹具，执行到卷根保留属性校验时停止。`-CatalogueProbe` 生成只读复查入口 `Partition-Merge-4Kn-Catalogue-Recheck.exe`，在验证两盘 GPT、两个目标目录均未创建及源文件内容后，用修正后的清单模块读取已验证的 A/9。配套 `Catalogue-Recheck-Report` 仅显示保存的记录。验收证据见 `.vm/partition-merge-batch-status.json`。

`-Corrected` 在源清单只读复查通过后验证文件迁移。正式程序 `d2e2080f…` 完成样本写入后在 Windows 还原原所有者时收到错误 1307。`-OwnerInspect` 核对两盘 GPT、源所有者与 DACL、源文件和暂存副本的内容。`-Fresh` 绑定所有者修正版，只有上述复查已通过才在 A 的空闲区创建一对新夹具。使用 `Fresh-Prepare` 准备，`Fresh-Check` 提交一次合并，`Fresh-Inspect` 独立读回，`Fresh-Report` 查看该次提交的结果。

## GPT 快速分区验收批次

后续进度/关联快照批次使用 `build-quick-progress.ps1` 和独立四个 `Quick-Progress-*` 入口。`Prepare` 只做 A/B 读取、基线记录及五步预览后作废令牌，不派发磁盘修改；只允许替换前一轮原生 UI 新建的 B DATA1（14768 MiB 起点/807 MiB），保留其余 18 项。正式程序原生 UI 将其分成默认 NTFS DATA1 403 MiB 与 DATA2 404 MiB；`Inspect` 独立只读核对精确范围、原始 NTFS 引导、卷标、全部保留元数据/GPT 与 B/15 文件，不写样本。两者均有独立跨构建 once 标记，`-Report` 只读日志。此批桌面 `80f98cb4…` 五步原生验收通过。原驱动 `c8dccc4c…` 将 GPT 数组槽位纳入比较并误报，原记录保留。`build-quick-progress.ps1 -Recheck` 单独生成只读复查入口，要求同一桌面散列与原失败记录。复查按唯一 GUID 核对保留条目全部字段，报告槽位 19→20，新建范围、NTFS 卷标及引导、A 完整 GPT、18 项保留分区元数据和 B/15 文件均通过，A/B 写入计数不变。见 `.vm/quick-progress-batch-status.json`。不要重跑历史 `Quick-Partition-*` 入口。

`build-quick-partition.ps1` 只编译客体驱动并绑定已完成的正式桌面 SHA-256，不在宿主执行。光盘入口 `Quick-Partition.exe` 在完整 UUID/三盘库存/容量/序列号检查后，要求 B 已在线且可写，使用固定跨构建 once 标记防止重复派发。先在 B 的未分配空间建立 NTFS/FAT32/exFAT/Linux 条目，再明确删除仅由本次创建的四个条目并重建 NTFS/FAT16。每组做独立文件逐字节/SHA-256 回读、原有全部分区元数据、A 完整 GPT、B 原有 GPT 字段与条目、B/15 保留文件核对。B/7 历史失败目标不重提，OS 不作为目标，不自动上线或回滚。

驱动随后只记录原生 UI 的另一组两分区计划及基线。`Quick-Partition-UI-Inspect.exe` 是单独的只读后验：核对精确布局、NTFS 卷标/原始启动扇区、保留 GPT 条目和 B/15 文件。`Quick-Partition-Report.exe` 与 `Quick-Partition-UI-Report.exe` 只读取各自日志，不重跑。ESP/MSR、4Kn 和并发/故障场景在此计划中仅有合成证据，不计入 VM 通过项。

首轮 `ba362a2e…` 驱动在 `resolve(stable=false, writing=false)` 未取得卷路径时停止，尚未派发任何批量预览/执行，A/B 写入计数未变。修正为取得稳定卷关联但仍 `writing=false`，随后只读打开保留文件。`Quick-Partition-Continue.exe` 校验原两条日志、构建/时间戳、原 once 标记和尚不存在的基线/原生计划文件，以独立跨构建 once 标记首次执行未开始的步骤；其 `-Report` 入口只读日志。原驱动和原报告保留，正式程序仍为 `266af3c1…`，不重建桌面程序、不重复已派发的变更。2 项定向续验约束检查通过，实际续验结果单独记录。

续验 `7973dcf1…` 已返回 PASS：7 步混合创建与 8 步仅替换本次新条目、5 次 65,553 字节文件读回、原布局/GPT/保留文件核对均通过。后续同版原生 UI 在 B 的 14768 MiB 起点/807 MiB 和 15575 MiB 起点/808 MiB 创建两个默认 NTFS 分区（DATA1/DATA2），保留现有全部 17 项。一次确认后四步均已核对，持久结果与刷新后的 19 项列表已观察；`Quick-Partition-UI-Inspect.exe` 独立只读后验也通过，期间 A/B 写入计数保持 420/2259 不变。源码冻结清单与正式程序三份散列复核一致。详细范围和未验收组合见 `.vm/quick-partition-continue-status.json`，不要重启原驱动、续验驱动或重复原生批次。

## 环境与边界

| 项目      | 配置                                                                                 |
| --------- | ------------------------------------------------------------------------------------ |
| 客体      | Windows Server 2025 Standard Evaluation 桌面体验，中文                               |
| QEMU      | 便携 11.1，TCG、4 vCPU、8 GiB、SeaBIOS、GTK                                          |
| 系统盘    | `system.qcow2`，80 GiB SATA，`DS-OS-0001`；不做分区测试                              |
| 测试盘    | `test-a.qcow2` / `test-b.qcow2`，各 16 GiB NVMe，`DS-TEST-A-0001` / `DS-TEST-B-0001` |
| 网络/集成 | 默认无网卡，无物理设备直通、共享目录、共享剪贴板                                     |
| 程序传入  | 只读工具光盘，路径与 SHA-256 由 `current-tools.json` 记录                            |
| QMP       | 仅 `127.0.0.1:49365`，核对名称 `DiskStudio-Test` 与固定 UUID                         |

**宿主只做 UI 和固定 SELECT 元数据检查。所有初始化、新建、格式化、缩放和删除测试都只在客体的 A/B 两盘执行。** 客体 Windows 返回的 NVMe 序列号由 `DS-TEST-A-0001`（或 B）、六个填充空格和 `_00000001.` 组成。验收按完整原始值精确匹配，不能单靠磁盘编号；界面排版可能合并连续空格。

恢复、备份、还原和克隆的原始设备读取与写入也遵守同一边界。宿主不得启动恢复扫描、预览候选文件或备份数据任务。

`guest-checks/` 是独立验收驱动，编译引用生产 Rust 引擎，不是另一个产品版本。只能通过只读工具 CD 在已校验的客体中运行；宿主只能编译，不能运行它。驱动启动及每个阶段检查固定 VM UUID、恰好三块磁盘、完整序列号、NVMe 类型与容量。它生成真实 NTFS 常驻/非常驻文件，删除自己的常驻样本，核对恢复字节，完成 2 GiB 备份/还原与分区克隆，最后执行 16 GiB A→B 整盘克隆。最后一次操作覆盖 B 上所有测试输出，并保持 B 脱机；不修改客体 OS 分区。

运行前先执行 `node scripts/vm/lab.mjs preflight`。驱动每阶段以原生对话框报告结果，失败时停止，不自动回滚。生产可执行文件仍须另行在客体检查 UI/IPC 流程；驱动测试不能替代同一份正式程序的验收。

如果客体重启或报告窗口被关闭，可从工具光盘运行 `DiskStudio-Format-Report.exe`。它先核对 VM 身份和三盘清单，再读取 `C:\ProgramData` 中最新的格式验收日志，核对文件名中的运行哈希与日志头；显示保存的完成报告，未完成时明确显示日志末尾。此入口不执行格式化或其他分区任务，也不会创建新的验收运行标记。核对报告中的哈希确实对应目标构建后，才能记录该轮结果；不要为找回报告重复启动格式验收。

`DiskStudio-Native-FAT.exe` 在 A/B 各自新建 2 MiB FAT12、128 MiB FAT16、512 MiB FAT32 分区，每个格式检查默认和自定义簇大小，共 12 组。只使用新分配范围，保留旧失败分区；每个驱动构建只允许启动一轮，日志以驱动 SHA-256 命名。格式化后的普通文件读回使用生产模块核对身份、布局和双向卷关联后得到的卷 GUID 根目录，支持没有盘符的卷；不调整已有盘符。

`DiskStudio-Native-FAT-Report.exe` 只读保存的原生 FAT 日志与当前盘符、B 盘分区元数据，不能启动测试。2026-11-12 的 ce9824… 运行完成 8 项后，在创建 B FAT16 时返回 40002，但只读核对发现 B/8 已经存在、没有盘符，文件系统显示未知；C–Z 已占用。因此不能重跑创建，也不能把未知当作 RAW。

`DiskStudio-Native-FAT-Finish.exe` 是该次现场的单次后续验收：必须匹配原运行的 8 个 PASS、失败上下文，以及 B/8 的 12903776256 起点、128 MiB 大小、RAW 和无盘符状态。先首次格式化已存在的 B/8，再只新建一个 512 MiB B FAT32 分区；各检查默认/自选簇及 512 KiB 文件读回，同时检查全部旧布局。创建新分区前要求 C–Z 仍全部占用，用于验证无盘符创建。独立固定运行标记防止重复，失败立即停止；不重试原来的创建或已完成格式化。该驱动通过前不能记为验收通过。

首个续测在预览时被旧分页文件保护拒绝：`finish-20261112.txt` 留下 `FAILED: PREPARE`，没有分区格式化或创建，A/B 写计数未增加。更新驱动必须先读到这个确切拒绝且没有 PASS/新建记录，才允许使用新的 `finish-guarded-20261112.txt` 单次标记，继续尚未执行的首个格式化。RAW 由绑定到 B/8 的原生卷句柄明确确认；不从界面的未知状态推断，不重复已经发出的存储变更。

`finish-guarded` 同样在 PREPARE 阶段因 RAW 卷 Win32 保护字段全部 NULL 而拒绝，A/B 写计数仍未增加。只读报告记录该事实。当前驱动要求前两份文件都匹配各自的明确预览拒绝且无 PASS/新建记录，才建立 `finish-resolved-20261112.txt`；生产引擎改为在明确分区保护标志之外，完整解析活动分页文件到规范卷 GUID。其后仍使用同一四项验收，失败不会自动重跑。

使用 `pwsh -NoProfile -File scripts/vm/build-guest-checks.ps1` 仅编译驱动，再按下文步骤制作并更换光盘。驱动静态链接 C 运行库，无需在客体额外安装运行库；不要在宿主启动生成的 `DiskStudio-Guest-Checks.exe`。

## 启动、登录和关机

### 未登记 NTFS 分区记录恢复验收（2026-11-13）

本轮 `fb26559e…` 正式程序已通过“搜索 → 浏览/五字节预览 → 恢复预览 → 确认写入 → 独立只读验证”。当前 A 为 19 分区，B 为 10 分区且保持脱机；B 新增的普通 NTFS 条目位于 LBA 29360128（14 GiB）、容量 2 MiB。完整文件系统与 `tests/fixtures/ntfs-testfs1.img` 一致，详情见 `.vm/lost-partition-restore-status.json`。历史要求 B 恰好 9 分区的工具不能直接用于当前状态，不要为满足旧前提重跑创建或删除任务。

`Lost-Fixture-Seed.exe` 只接受 A/19、B/9 和准确的空白范围，核对锁卷、原生布局及首尾表窗口后准备 2 MiB 文件系统；不删现有分区，也不写 GPT。当前已经准备并恢复完成，禁止再次运行。`Lost-Fixture-Inspect.exe` 是准备阶段的 B/9 只读检查；`Lost-Fixture-Verify.exe` 则要求当前 B/10、脱机，核对两份 GPT、原生布局和整个文件系统字节。三个入口均只限已核对的隔离 VM，不要运行其他驱动来代替这些精确模式。恢复本身由正式程序操作，验证工具不调用恢复实现。

### Ext2 / Ext3 本轮验收

`DiskStudio-Ext-Format.exe` 仅在 A/B 各自至少 3 GiB 的未分配范围内创建两块新的 1 GiB Linux 分区，分别格式化为 Ext2、Ext3；A 使用 1/2 KiB 块，B 使用 4 KiB 块。不会重格式化已有 ext4、Windows 或备份分区。`C:\ProgramData\DiskStudio-ext23-fixtures.json` 保存新分区的 GUID、起点和其他分区基线；存在记录即拒绝重复格式化，失败不自动重试。

Windows 正常关机且进程退出后，`ubuntu.mjs seed-ext-populate` / `ext-populate` 使用独立 Ubuntu 写入文件基线；Ubuntu 自动正常关机后才可重新启动 Windows。`DiskStudio-Ext-Resize.exe` 使用同一生产模块完成 1 GiB → 384 MiB → 1.5 GiB。再次关机后运行 `seed-ext-verify` / `ext-verify`，核对实际文件系统类型/块大小、文件 SHA-256、权限、UID/GID、xattr、硬链接/符号链接、稀疏洞以及文件系统/分区 UUID。新种子只读、不覆盖旧种子，仍通过 QMP 实时核对精确文件后端、UUID 和容量。本构建尚未执行该 VM 流程。

```powershell
node scripts/vm/lab.mjs doctor
node scripts/vm/lab.mjs start
node scripts/vm/lab.mjs status
node scripts/vm/lab.mjs io-stats
node scripts/vm/lab.mjs shutdown
```

也可双击 `Start-VM.cmd` / `Shutdown-VM.cmd`。关机命令只发送 ACPI 请求，应等 QEMU 完全退出。不要在正常安装或写盘中强制关闭。`poweroff` 仅供卡死时恢复。

`io-stats` 在相同身份和文件后端检查通过后，只查询 QEMU 的累计 I/O 计数，便于观察长任务是否继续推进；它不读取磁盘扇区，累计值也不代表某个任务已经通过。

`pause-vm` / `resume-vm` 在核对 VM 名称、UUID、三块固定文件后端和容量后，分别请求 QMP `stop` / `cont`，只用于给快速任务留出界面观察时间，不接受额外参数。QEMU 暂停时可能丢弃输入；应用内暂停必须在恢复 VM 后点击并核对实际保存位置。测试结束应保持 VM 正常运行，不能把 hypervisor 暂停当作应用续扫功能验收。

当前 Windows 客体已通过微软 Sysinternals Autologon 配置本机 Administrator 自动登录。2026-11-12 实测 Windows 正常重启和完全关机后重新启动均自动进入桌面，两次验证未发送登录按键。密码由用户在客体工具界面输入，使用 LSA secret 保存；项目脚本和命令行不保存密码。工具来源：[Microsoft Autologon](https://learn.microsoft.com/en-us/sysinternals/downloads/autologon)。

本轮正常关机后三块 QCOW2 一致性检查均为 0 错误。并行进行的开发任务在 2026-11-12 02:52（UTC+08:00）保存了包含自动登录配置的三盘检查点 `before-ext-fat`，随后启动其测试；本任务已核对该检查点的 UUID、三盘清单及下一次启动 QMP 清单中的全部快照标签，因此复用这个已保存状态，未打断后续测试。修改前检查点为 `before-autologon-20261112`；恢复旧状态或重新安装后仍可能需要手动登录。`Guest-Login.cmd` 或 `lab.mjs login-keys` 在确认 QMP 身份后仅向客体发送 Ctrl+Alt+Delete，不接受、保存或填写密码。

本轮只读检查显示屏保未启用、机器/MDM 闲置锁定策略未配置、睡眠超时为 0（从不），显示关闭超时为 600 秒；未修改这些闲置设置。自动登录只负责开机登录，不负责解锁手动锁定的会话。配置与验证记录保存在 `.vm/autologon-status.json`，只读客体检查工具及源码也只保存在 `.vm/`。

本机 WHPX 曾出现启动/重启故障，默认 TCG 较慢；不为加速测试改变宿主 Windows 功能或安全设置。

## 更新同一份程序

```powershell
# 只构建一次；普通发布构建，不带特殊功能开关
pwsh -NoProfile -File scripts/vm/build-app.ps1

# 从 target/release/diskstudio.exe 制作新的只读工具光盘
pwsh -NoProfile -File scripts/vm/make-tools-iso.ps1 -Live

# 退出客体内旧程序，替换只读工具光盘
node scripts/vm/lab.mjs refresh-tools

# 每轮真实操作测试前核对环境
node scripts/vm/lab.mjs preflight
```

构建输出 `target/release/diskstudio.exe`，测试留档副本为 `.vm/artifacts/DiskStudio.exe`。工具盘中的 `DiskStudio.exe` 是同一文件，`build-info.json` 记录生产构建 SHA-256。可在客体从光盘直接运行，或复制到桌面。历史 `DiskStudio-Host.exe` / `DiskStudio-VM.exe` 留在 `.vm/artifacts/` 供追溯，不再由构建脚本生成或使用。

`make-tools-iso.ps1` 使用 IMAPI 文件系统镜像 API 创建普通 ISO 文件，不挂载、不刻录，不访问宿主物理光驱。每次 `-Live` 生成独立 ISO，旧文件仍留在 `.vm/media/`。为单批验收制盘时，可追加 `-GuestTools` 并传入来自 `.vm/payload/` 的驱动文件名数组，只包含正式程序和选中的驱动；无需移动或删除旧驱动。热切换核对 QMP 身份、三块固定 QCOW2 后端、唯一只读光驱、新文件路径/类型与 SHA-256，不增加共享目录或网络。

WebView2 安装器仅供客体需要时使用；当前客体已有可用运行时，宿主无需安装。

## 功能验收

### 4Kn 拆分、空间重新分配与左侧合并

`build-partition-reallocation.ps1` 绑定已完成的正式程序 SHA-256，只在宿主编译驱动。使用 `start --4kn`、`preflight` 和只读工具光盘。来宾入口再次核对 UUID、固定三盘、命名空间 1 的完整 A/B 序列号以及两种扇区大小。首次 `Partition-Reallocation-4Kn-Prepare.exe` 需要 A/B 已联机可写，且 A 的 2048–5504 MiB 全部空闲。它只在该范围创建八个带 131,089 字节样本的 NTFS 分区，核对 B 的原有断点卷 `DS4KMOVEJRN`，保存 GPT 基线。已有一次性标记时停止，不清理、不覆盖或重跑。

在实际桌面程序中按下表依次预览和确认。每步通过后打开对应的来宾检查器。预览与执行只能选已经核对序列号的 A；搬移时原生文件夹对话框选择 B 的 `DS4KMOVEJRN`。从跨分区分配开始，成功后程序应让 A 保持脱机。先完成脱机只读检查，再单独确认联机以准备下一项。失败时检查保存结果和真实布局。

| 来宾检查器后缀 | 原生操作 | 预期范围 |
| --- | --- | --- |
| `Split` | 拆分 `DSREALLOCSPLIT`，尾部 64 MiB 采用 NTFS、默认簇和 `NEWVOLUME` | 原卷起点 2048 MiB、缩至 320 MiB；新卷 2368 MiB、64 MiB |
| `Adjacent` | 给 `DSREALLOCADJ` 分配右侧 2688 MiB 开始的 32 MiB 空闲空间 | 保持起点 2560 MiB，增至 160 MiB |
| `Across` | 给 `DSREALLOCGROW` 分配 3328 MiB 开始的 64 MiB 空闲空间 | 中间卷 `DSREALLOCMOVE` 起点 3200→3264 MiB；接收卷增至 192 MiB |
| `Transfer` | 从 `DSREALLOCDONOR` 向 `DSREALLOCRECV` 转移 64 MiB | 供给卷缩至 320 MiB；接收卷移至 4416 MiB，再增至 192 MiB |
| `Merge-Left` | 保留 `DSREALLOCKEEP`，吸收左侧 `DSREALLOCLEFT` | 左卷删除，保留卷移至 5120 MiB、增至 384 MiB，吸收样本位于新合并目录 |

检查器通过主备 GPT 和 Windows 原生布局核对 A 的保存基线与预期变化，逐字节验证所有样本及 SHA-256，确认 B 的 GPT 和客体 OS 的布局元数据不变。检查器只读取测试盘；同名 `-Report` 只显示已有日志。测试夹具接通和运行结果单独记录。

如果 QEMU 窗口未接收宿主键盘输入，先在来宾界面聚焦分配容量栏，再运行 `node scripts/vm/lab.mjs reallocation-capacity-keys 32` 或传入 `64`。命令复核固定 4Kn 来宾与完整设备清单，只向来宾输入对应数字。提交前仍须在原生预览核对磁盘、分区和每项步骤。

向左合并若在暂存后因保留卷锁失败而停止，使用 `build-partition-reallocation.ps1 -InspectMergeLockFailure` 生成单独的只读检查器及 `-Report`。检查器要求 A/B 联机，按 `Transfer` 成功后的布局核对分区与全部原始样本，另外在保留卷新目录中回读被吸收卷的样本。此检查既不复提合并，也不修改卷状态。

`build-partition-reallocation.ps1 -FreshLeftMerge` 生成 `Partition-Reallocation-4Kn-Left-*` 批次，用于已经完成 `Transfer` 且历史左合并已核对停留在原布局的 4Kn 来宾。`Left-Prepare` 验证历史 GPT 基线和空闲范围，只在 A 的 5632–6528 MiB 内一次性创建两对普通 NTFS 分区与四份独立样本。验证 B 的断点卷后，按以下顺序用同一桌面程序测试。

1. 启动 `Left-Hold` 并保留提示窗口。这会持有 `DSBUSYKEEP` 上一个已核对的只读文件句柄。在桌面程序中保留 `DSBUSYKEEP`、吸收左侧 `DSBUSYFROM`，断点选择已核对的 B 卷。记录保留卷锁失败结果后，关闭持有窗口。`Left-Busy` 独立核对未变化的 GPT、原始样本和暂存副本。
2. `Left-Busy` 通过后，在另一对分区上保留 `DSLEFTKEEP`、吸收左侧 `DSLEFTFROM`，仍用 B 保存断点。`Left-Success` 核对 A 保持脱机、源记录移除、保留卷起点由 6272 移至 6144 MiB 并扩至 384 MiB、两阶段的合并目录及原样本和非目标 GPT。

对应的 `-Report` 仅读保存结果。旧 A/16 与 A/17 和新用例的失败状态留作核查，每次合并在桌面程序中只提交一次。

首轮 `Left-Prepare` 在来宾意外重启时停在第二块卷的格式化，留下第一块完整夹具。`build-partition-reallocation.ps1 -ResumeFreshLeft` 只生成针对该中断的检查和续验入口。`Left-Interrupted` 绑定原桌面与原驱动散列、完整日志序列、历史 GPT、两块已创建范围和剩余空闲范围，并读取第二块卷的 NTFS 主备引导、干净状态和样本状态。通过后 `Left-Resume` 复核相同证据，仅在样本不存在时首次创建样本，并准备未曾提交的两块分区。使用原来的 `Left-Hold`、`Left-Busy`、`Left-Success` 完成后续验收。

占用用例通过了安全停止检查。没有测试占用句柄的 A/22 与 A/23 也在复制后遇到保留卷锁失败。`build-partition-reallocation.ps1 -InspectCleanStop` 生成单独的只读 `Left-Clean-Stopped` 及其报告入口，核对两盘 GPT、来宾 OS 布局、两对新卷的原始样本和两份暂存副本。记录见 `.vm/partition-reallocation-4kn-fresh-left-status.json`。

新正式构建使用 `build-partition-reallocation.ps1 -RevisedLeftMerge`。`Left-Revised-Prepare` 先核对 Alpha 的 GPT 基线、原始样本及两份暂存副本，然后仅在 A 的 6656–7552 MiB 新建四个夹具。依次用 `Left-Revised-Hold` 验证 `DSRVBUSYFROM` 并入 `DSRVBUSYKEEP` 的占用路径，再用 `Left-Revised-Busy` 独立核对。干净路径在 `DSRVLEFTFROM` 与 `DSRVLEFTKEEP` 上执行，同样将断点保存在 B。成功后 `Left-Revised-Success` 核对保留卷移至 7168 MiB、扩至 384 MiB 且 A 脱机。若复制后在保留卷锁处停止，则运行 `Left-Revised-Clean-Stopped` 核对保留的两卷和暂存副本。各检查均比较非目标 GPT 与 Alpha 样本。

修订版 `71f10fa9…` 的占用用例通过安全停止与独立回读。无测试占用句柄的 A/26 并入 A/27 仍在保留卷锁处遇到 Windows 错误 5。两次测试均保留原分区、样本与暂存副本，B 的写入计数保持 224，独立回读期间 A/B 没有新增写入。见 `.vm/partition-reallocation-revised-status.json`。

`build-partition-reallocation.ps1 -ProbeRevisedLeftLock` 只生成独立卷锁探测和报告入口，复用同一桌面散列。关闭来宾 DiskStudio 后运行 `Left-Revised-Lock-Probe`。它先核对隔离配置、已保存的 GPT 基线及暂存样本，再短暂尝试锁定 A 上的固定目标卷。它分别探测原占用目标单独锁定、无占用目标单独锁定和持有源卷锁时锁定无占用目标，最后复核三盘布局。探测不提交合并。

三个独立锁探测均通过。`-ProbeLockAfterCopy` 在 A 的 7680–8064 MiB 空闲区新建两卷，只执行正式 NTFS 复制和卷锁探测。复制关闭文件句柄之后，立即锁定收到 Windows 错误 5，约 251 毫秒时锁定并回读副本成功。随后独立核对 GPT 和样本。

`-LockGrace` 绑定加入 500 毫秒等待的正式程序。在 A 的 8192–9088 MiB 空闲区准备两对新夹具。按 `Grace-Prepare`、`Grace-Hold`、`Grace-Busy` 的顺序验证 `DSGBUSYFROM` 并入 `DSGBUSYKEEP` 时的占用路径。正常路径保留 `DSGLEFTKEEP`，吸收 `DSGLEFTFROM`，断点仍在 B。`Grace-Success` 核对保留卷移至 8704 MiB、增至 384 MiB、A 脱机、样本字节以及非目标 GPT。失败时选择符合实际结果的只读检查器。

正式程序 `9a616414…` 的两种路径均通过 4Kn 原生验收。A/30 并入 A/31 在占用期间安全停止。A/32 并入 A/33 完成复制、核对、删除、移动、扩容和最终脱机。独立回读核对两盘 GPT、来宾 OS 布局、原样本及暂存副本，回读期间 A/B 无新增写入。见 `.vm/partition-reallocation-grace-status.json`。

### 固定盘符与操作结果修复批次

`build-letter-outcome.ps1` 仅编译同一生产模块并绑定已完成的桌面 SHA-256。`Letter-Outcome.exe` 验证 UUID、三盘清单、完整序列号和 B/15 固定 GUID/范围/NTFS/卷标，要求 A、B、Z 盘符空闲。唯一新建 64 KiB 校验文件后，依次在 B/15 执行无盘符→A→B→无盘符，每一步独立读取校验文件及 SHA-256、完整 A/B GPT、全部分区布局和其他盘符。跨构建固定 `.once` 防止重复派发；异常停止，不清理、不格式化、不联机、不重试 B/7 原失败。`Letter-Outcome-Report.exe` 只读保存结果。随后可在同一正式程序中独立确认 B/15 无盘符→Z，`Letter-Outcome-UI-Inspect.exe` 仅仅读取核对该结果及原校验文件；`Letter-Outcome-UI-Report.exe` 只读其日志。实现、编译及内存测试不等于客体验收，状态见 `.vm/letter-outcome-batch-status.json`。

固定盘符批次验收完成：`80e7ef1a…` 正式程序与 `90c9e929…` 驱动通过 B/15 无盘符→A→B→无盘符的三个生产操作；原生 B/15 无盘符→Z 的预览、单次确认、持久结果及独立只读后验也通过。每步校验文件内容/SHA-256、GPT、全部布局及其他盘符，B/7 原失败保持不动；只读后验期间 A/B 写入计数不变。异常刷新/目标消失/未核对结果仍仅有合成检查证据，详细范围见 `.vm/letter-outcome-batch-status.json`。

### NTFS 命名数据流批次

`build-ntfs-streams.ps1` 只编译并绑定已核对的桌面散列，生成 `Ntfs-Streams.exe` 与仅读保存报告的 `Ntfs-Streams-Report.exe`。通过隔离检查后，客体驱动只在 A 的既有 2 GiB NTFS 创建唯一新文件及命名流，在 B 的既有 6 GiB NTFS 创建唯一输出目录；不格式化、删除或覆盖历史夹具。原生创建/读回与原始 NTFS 引擎独立比较驻留、空流、普通、稀疏、LZNT1 和扩展记录，验证进度序列化、平面/树状导出以及每个实际输出的字节与 SHA-256；前后比较完整 GPT 和所有分区布局。错误即停止，不重试，日志按驱动散列 create_new 保存；原生桌面 UI 另行检查。实现及编译不等于客体验收通过。

2026-11-16 首轮 `c93dbf7f…` 驱动在 24 个样本的原生读回通过后，因稀疏流字段检查停止；B 导出尚未开始，不能重跑初始驱动。`-Inspect` 只生成新的只读诊断入口；`65a1e620…` 已核对固定失败日志和既有记录，确认稀疏流单元 4、16 簇末尾对齐和零保留字段，诊断期间 A/B 写计数不变。首次失败及诊断分别保留于 `.vm/ntfs-streams-status.json`。

修复后使用 `build-ntfs-streams.ps1 -ContinueExisting`，只生成 `Ntfs-Streams-Continue.exe` 及同名 `-Report`，不覆盖首轮/诊断入口。它核对固定四条日志、固定文件名和唯一的 A/2 GiB NTFS 源目录，重新以只读原生文件句柄核对全部既有样本，不重新创建或写入来源。之后用修复引擎检查扫描/进度/提取，并首次在 B 执行平面、树状导出。固定 `.once` 标记跨驱动构建阻止重复派发；失败停止，不自动重试。产品没有 VM 开关，驱动只编译同一生产模块。

2026-11-17：0a82c2d1 正式构建与 528e803d 续验驱动完成 24 个保留源样本、原始提取/进度往返、平面和树状共 48 个输出的独立字节/SHA-256 核对，GPT/全部布局不变。原生命名流列表及驻留、压缩预览通过。随后 B/7 的 Z 盘符移除成功，恢复 Z 的独立操作收到 AddAccessPath 参数无效（5），未重试，B/7 保持无盘符。成功/失败结果弹窗均持久显示；这一盘符缺陷不覆盖已通过的数据流验收。具体范围见 `.vm/ntfs-streams-corrected-status.json`。

### 完整格式化与 NTFS 选项批次

`build-format-options.ps1` 仅编译客体驱动，将正式桌面 SHA-256 固定到该驱动；不在宿主运行它。通过只读工具 CD 在通过 QMP 隔离检查的客体中启动 `Format-Options.exe`。它每阶段复查 UUID、三盘清单与完整序列号，只在 B 的未分配区域创建五个普通 GPT 数据分区（2/32/64/64/128 MiB），不格式化历史分区。五种文件系统各检查快速和完整格式化；NTFS 额外关闭压缩及短文件名并切换至 64 KiB 簇，共 11 例。每例验证 524,305 字节文件内容，NTFS 检查新文件压缩继承，并比较格式化前后的完整 GPT 与全部分区布局。

驱动按自身 SHA-256 保存单次运行标记与逐阶段日志；失败即停止，不重试格式化或创建。`Format-Options-Report.exe` 只读取同一驱动的保存报告。FAT 完整范围回读由生产引擎核对；Windows 完整格式化验证提供者成功与写后文件系统状态，不宣称已独立证明卷内每字节为零。正式桌面 UI 需要单独验收；尚未运行的例子不能记为通过。当前批次状态见 `.vm/format-options-batch-status.json`。

2026-11-16 的 `752da6a3…` 驱动完成 8 例后，在新建 B/15 的 NTFS 快速格式化后状态查询失败，不能重跑初始驱动。`98f700a6…` 的 `Format-Inspect.exe` 绑定该日志、GUID、13744734208 起点和 128 MiB 大小，只读诊断确认零访问卷句柄返回错误 1，而只读句柄可返回状态；响应只设置 VolumeFlags，其余字段为零。A/B 写入计数不变。

修复后用 `build-format-options.ps1 -Continue` 只生成 `Format-Options-Continue.exe` 及同名 `-Report`，保留原驱动。后续驱动先用修复后的生产查询核对已有 NTFS 结果，并完成之前未进行的文件校验，不重提该快速格式化。随后只执行剩余的完整/压缩/短名开启与快速/64 KiB/关闭用例。固定 `.once` 标记防止更换构建后重复续测；失败继续停止。结果见 `.vm/format-correction-status.json`，旧 8 例不会自动算作新构建重测结果。

`7d2ea89b…` 修复桌面与 `f5b96749…` 驱动的上述后续验收已通过，文件内容、压缩继承及 GPT/布局均通过。随后同一正式程序在 B/15 完成一次独立的原生完整格式化流程，选项为 4 KiB、压缩和短名开启。最终通知未捕获，不能把窗口关闭当作 verified=true 的证据。将同一已编译驱动复制为 `Format-Inspect.exe` 后，通过新的只读工具 CD 执行单次只读查询：根目录属性 0x816、压缩 [2,0]；只读卷句柄返回 16 字节零状态（短名开启），范围匹配。诊断前后 A/B 写入计数不变。该过程没有再次格式化、重新编译产品或重跑旧用例；诊断日志按新的驱动散列独立保存。

1. 运行 `preflight`：确认固定 VM UUID/名称、恰好三块普通文件后端、容量和只读光盘。
2. 在客体 UI 核对 A/B 完整序列号和 16 GiB 容量。系统盘及其分区不得作为测试目标。
3. 使用同一生产程序验证初始化、新建、NTFS 格式化、调整大小、删除，以及系统/保留分区保护。每次执行观察结果核对，失败不自动重试写入。
4. 先正常关机，再检查 QCOW2 一致性并保存检查点；记录构建哈希及实测结果。

应用本身按通用规则保护目标：唯一设备身份、跨 Windows 提供程序对应、布局完整性、只读/脱机/群集状态、具体分区的系统/启动状态、类型及卷关联。系统盘的普通 GPT 数据分区可管理，运行中的 Windows NTFS 卷只允许调整大小。Linux GPT 数据分区支持明确确认后的 ext4 格式化与末端缩放，范围见项目 README。单次确认绑定布局和卷身份，5 分钟过期；NTFS 调整范围由 Windows 检查。不会自动提权，不承诺事务回滚。测试目标始终仅限 A/B 两盘。

历史第一阶段 `partition-management-ready` 检查点包含专用旧版本：A 盘 2 GiB NTFS / D:，文档在 2→3→2 GiB 调整后重新打开核对；B 盘完成创建/格式化/删除，保留 MSR。该阶段记录 `.vm/partition-management-status.json`。单一正式程序的回归结果记录 `.vm/unified-app-status.json`。

## 检查点和自动检查

```powershell
node scripts/vm/lab.mjs checkpoint before-test
node scripts/vm/lab.mjs restore before-partition-tests
node --test scripts/vm/lab-tests.mjs
# smoke 只在关机状态运行
node scripts/vm/lab.mjs smoke
```

检查点与恢复只允许 VM 停止后执行，操作全部三块盘。恢复前自动保存 `before-restore-时间戳`，名字不能覆盖旧检查点。`before-partition-tests` / `clean-install` 是历史程序和两块 RAW 测试盘，恢复后应从当前光盘更新程序。

路径验证拒绝设备路径、越界、硬链接、符号链接和目录联接；QCOW2 禁止外部 backing/data 文件。检查点不完整时不会发布可恢复清单。管理锁为 `.vm/locks/manage.lock`，只在确认对应进程已经退出后才能清理遗留锁。

## 清理和重建

**正常关闭 Windows 和 Ubuntu 两台 VM，确认进程均已退出后，删除整个 `.vm/` 即可清理 QEMU、下载、工具链中间文件与全部虚拟机数据。** 没有另外散落在 Downloads 或 Program Files 的副本。保留 `scripts/vm/` 可重建。产品需要的 ext4 引擎资源和许可证源码分别保留在 `src-tauri/ext4-engine/`、`third-party/e2fsprogs/`，它们不是测试镜像。

只释放虚拟盘时，关机后可一起删除 `.vm/windows`、`.vm/checkpoints`、`.vm/run.json` 再运行 `init`；不能拿旧检查点清单恢复新盘。

```powershell
pwsh -NoProfile -File scripts/vm/setup-qemu.ps1
node scripts/vm/lab.mjs install
```

安装阶段只暴露系统盘，测试盘稍后接入。ISO 位于 `.vm/media/windows-server-2025-zh-cn.iso`。下载源与哈希检查保留在安装脚本中。

实现依据：[QEMU 光盘热切换](https://www.qemu.org/docs/master/interop/qemu-qmp-ref.html#command-blockdev-change-medium)、[Initialize](https://learn.microsoft.com/en-us/windows-hardware/drivers/storage/initialize-msft-disk)、[CreatePartition](https://learn.microsoft.com/en-us/windows-hardware/drivers/storage/createpartition-msft-disk)、[Format](https://learn.microsoft.com/en-us/windows-hardware/drivers/storage/format-msft-volume)、[Resize](https://learn.microsoft.com/en-us/windows-hardware/drivers/storage/msft-partition-resize)、[GetSupportedSize](https://learn.microsoft.com/en-us/windows-hardware/drivers/storage/msft-partition-getsupportedsizes)、[DeleteObject](https://learn.microsoft.com/en-us/windows-hardware/drivers/storage/msft-partition-deleteobject)。

## 历史：单一程序回归（2026-11-11）

生产程序 SHA-256：`83277d5de60c187ea13e6927db9cfe3cb39662d874cc49b6562f250d38b54f4e`。本地输出、测试留档、工具光盘 payload 一致；在客体直接运行该光盘中的程序完成验收。

40 项自动测试通过，前端生产构建、Rust 格式检查和桌面构建通过。客体 A 盘完成 GPT 初始化、创建 2 GiB / D:、NTFS 格式化、2→3→2 GiB 扩缩容，Windows 分区及卷容量核对成功。B 盘完成 GPT 初始化、创建 2 GiB / F:、删除未格式化的数据分区，F: 消失、未分配空间恢复、MSR 保留。随后复查 A 盘布局与客体系统盘原有三个分区，系统盘操作入口保持保护。

本轮是通用后端架构回归，验证了界面、目标绑定与操作结果；未执行宿主分区变更，也未进行断电恢复或大数据量压力测试。完整记录见 `.vm/unified-app-status.json`。

验收后已正常关闭客体，三块 QCOW2 一致性检查均为 0 错误，保存 `unified-production-ready` 检查点。客体桌面已更新为本轮生产程序。本地最新界面已重新打开，未提交宿主分区操作。

## 分区能力与导航更新验收（2026-11-11）

本轮生产程序 SHA-256：`d42a6c92b176dcd29a8459560dea317bfe19da1a52fca390b99255d03e9e69dc`，本地输出、测试留档与光盘 payload 完全一致。42 项自动测试、前端构建、Rust 格式检查和桌面构建通过。

仅在 VM A/B 两盘执行四次真实写入：B 盘创建并删除 2 GiB Linux GPT 表项，恢复 15.98 GiB 未分配空间并保留 MSR；A 盘 NTFS 完成 2→3→2 GiB 调整，界面核对分区和卷容量一致。系统卷缩放规则仅用合成元数据测试，未对客体 OS 盘或宿主磁盘执行分区操作。Linux 测试未建立 ext4 文件系统。

原生界面验证了顶部磁盘菜单、合成 GPT 镜像导入/关闭、镜像与本机磁盘同列、示例二级菜单、返回磁盘管理及精简设置页。导航参考 [DiskGenius 的磁盘菜单](https://www.diskgenius.com/manual/operate-virtual-disk.php) 和 [Windows 磁盘管理的操作菜单](https://learn.microsoft.com/en-us/windows-server/storage/disk-management/manage-virtual-hard-disks)。

客体桌面程序已更新，正常关机后保存 `partition-capabilities-ready` 检查点，三块 QCOW2 一致性检查均为 0 错误。完整记录：`.vm/partition-capabilities-status.json`。

## 文件恢复与数据复制验收（2026-11-11）

当前生产程序 SHA-256 为 `4ee2f060e1072e14e1bea1ba9daed55193a5a8d3806f7706d85f40333e3d2b64`，客体桌面、工具光盘和本地输出已同步。54 项自动检查通过。

客体验收驱动通过 NTFS 删除文件/非常驻文件恢复的字节比对、PDF 签名提取、2 GiB 分区备份与还原、2 GiB 分区克隆、16 GiB 整盘克隆及 SHA-256 读回校验；客体 OS 布局保持不变。正式程序还独立跑通扫描、预览、导出、备份与确认流程。测试阶段和两个仅前端不同的构建记录见项目 README 与 `.vm/data-features-status.json`。

已正常关机，三块 QCOW2 检查均无错误，保存 `data-features-ready`。B 最终是 A 的完整副本并保持脱机，原 B 上备份与恢复输出已被整盘克隆覆盖。需要重新运行完整驱动时，应在停止状态恢复 `partition-capabilities-ready`，再启动、由用户登录并使用当前工具光盘程序；不要直接把克隆后的 B 当作空白测试盘继续创建分区。

## 历史验收：并行备份与 FAT/exFAT（2026-11-11）

该阶段正式构建 `3d476d84beccddac1131a46d1c4d7e38f37ad20dc59f673ea5d6f1a4f4ff492c`，本地输出、留档与光盘 payload 相同，客体桌面已从光盘更新。工具光盘 `diskstudio-tools-30c63290-1287-486a-8d89-ff37da0765c0.iso`；验收驱动哈希 `16917d5c76277ada9aaccedb3e0bd96daca90e34a3defcd104d01c749bac1dc0`。63 项自动检查、前端和桌面构建通过。

驱动入口为 `guest-checks/src/advanced.rs`，先校验固定 UUID、三盘完整身份与容量，才在 A/B 建立夹具。A 原 2 GiB NTFS 用于 VSS/备份/还原，另建 256 MiB FAT32 与 512 MiB exFAT；B 建立 6 GiB 备份卷与 2 GiB 克隆目标卷。FAT32/exFAT 现存和删除长文件名恢复、打开文件的 VSS 快照一致性和自动释放、热备、压缩完整备份及两层增量、完整链从 B 还原到 A、2 GiB A→B 克隆与读回校验、预先取消任务均通过。客体 OS 布局不变，A 在线、B 脱机；B 的 6 GiB 卷保留输出，本轮未进行整盘克隆。

备份文件必须位于还原目标以外的磁盘。本轮早期脚本错误地从 B 上文件还原 B，卷锁检查拒绝执行；修正后按“备份到 B → 还原 A → 克隆 A 到 B”完整通过。驱动可识别并复用准确匹配的 FAT32/exFAT 夹具，但不自动重试失败操作。重新开始独立验收时仍建议先关机、恢复 `partition-capabilities-ready` 并手动登录，避免沿用未知中间状态。

`build-guest-checks.ps1` 将 `crt-static` 应用于整个依赖图，包含 Zstd C 库，避免客体缺少 VC 运行库。驱动在宿主只编译，绝不能运行。登录后先 `node scripts/vm/lab.mjs preflight`，再通过客体资源管理器启动光盘驱动。正式程序另行验证了 FAT32 删除扫描、文本预览、备份选项与 VSS 条件、脱机磁盘保护。完整记录见 `.vm/advanced-data-status.json`。

验收后已正常关机，三块 QCOW2 一致性检查均为 0 错误，保存 `advanced-data-ready`。该检查点包含本轮正式程序、A 的三种文件系统夹具，以及 B 上的备份输出和脱机克隆目标；不是空白盘基线。全部资源仍位于 `.vm/`，关机后可统一清理。

## Ubuntu 独立 ext4 验证

Ubuntu 24.04 cloud image、16 GiB 独立系统盘、只读 NoCloud seed 和串口日志均在 `.vm/ubuntu/`。镜像哈希固定在 `ubuntu.mjs`，Ubuntu UUID 为 `25c39913-ac45-4445-aa54-bdd0dbca7001`。无网卡、共享目录、剪贴板和设备透传；不需要客体登录。配置以暂停状态启动，核对 QMP 身份、精确文件后端、容量和只读 seed 后才继续启动。TCG 启动约需数分钟，网络等待服务超时不影响离线测试；测试脚本完成后自动正常关机。

A/B 的 QCOW2 在 Windows 与 Ubuntu 间轮流使用，不并发连接。两个入口均拒绝在另一台 VM 运行时启动；`lab.mjs` 的检查点也要求两者均停止。`ubuntu.mjs stop` 是紧急强制退出，只用于尚未运行测试的启动诊断，不能用于中断文件系统写入。

阶段命令：

```powershell
# 仅首次准备独立 Ubuntu 系统盘和 probe seed
node scripts/vm/ubuntu.mjs init
node scripts/vm/ubuntu.mjs probe
# Windows 格式化 A/B 后正常关机，Ubuntu 写入独立数据基线
node scripts/vm/ubuntu.mjs seed-populate
node scripts/vm/ubuntu.mjs populate
# 等待自动关机，再启动 Windows，手动登录，执行缩放
# Windows 正常关机后，Ubuntu 只读挂载并独立比对
node scripts/vm/ubuntu.mjs seed-verify
node scripts/vm/ubuntu.mjs verify
```

Seed 不覆盖已有文件。Populate 只接受各一个 2 GiB ext4 Linux 数据分区和准确标签；Verify 要求缩放后各为 3 GiB。客体脚本另核对 DMI UUID、三盘序列号/容量与根分区归属。写入基线包括中文文件名、160 MiB 多 extent 文件、硬/符号链接、稀疏文件、碎片分配、UID/GID、权限和 xattr；清单保存在 Ubuntu 自己的系统盘。验证要求文件 SHA-256、元数据、链接关系、稀疏洞、文件系统 UUID 和分区 UUID 全部一致，并在挂载前后执行 Linux e2fsck。

Windows 验收驱动只在宿主编译。`DiskStudio-Ext4-Online.exe` 只联机两个固定测试盘并确认 2 GiB 夹具；通过正式程序把 A 缩到 0.5 GiB 后，运行 `DiskStudio-Ext4-After-UI.exe`，它先要求 A 已为 512 MiB，再把 A 扩至 3 GiB，对 B 执行 2 GiB → 512 MiB → 3 GiB。不要重复运行已完成或失败的驱动；先检查阶段结果与磁盘状态。独立全驱动入口 `DiskStudio-Ext4-Resize.exe` 要求两盘仍为 2 GiB。工具光盘中的格式化驱动会清除指定测试分区，仅可在明确的基线阶段使用。

当前阶段结果和构建身份记录在 `.vm/ext4-status.json`；出现 PASS 之前不算通过。Ubuntu 日志中的正常网络等待失败与测试失败不同，验收必须看到 `DISKSTUDIO_EXT4_ALL_VERIFY_PASS`。

## 当前 ext4 验收结果（2026-11-12）

最终正式构建 `a9534e648c230098ddc3d16e6b03dbd29ed42f8693a1ba5399e662130a7eab66`，本地、留档和光盘 payload 相同；客体直接从光盘启动，旧桌面副本未覆盖。工具光盘 `diskstudio-tools-45187736-8c07-4dc9-80c0-73c1a2c2e80c.iso`，驱动 `1f10ef4bef003da6e5d6ada12d185055af05278b6c7538481f07e40658620149`。

A/B 两个 2 GiB ext4 的创建、Ubuntu 基线写入、2 GiB → 512 MiB → 3 GiB 扩缩容、Ubuntu 只读挂载后的完整 SHA-256/元数据比对全部通过。A 的缩容通过正式 UI 提交，其余缩放由包含同一生产模块的驱动执行。Linux 文件系统检查前后均通过，文件与分区 UUID 保留，客体 OS 布局未变。最终日志 `.vm/ubuntu/verify-1794413067418.log` 包含两盘 PASS 和 ALL_VERIFY_PASS。

两台 VM 均已正常关机，Windows 三盘及 Ubuntu 系统盘的 QCOW2 检查均无错误，保存 `ext4-ready` 三盘检查点。检查点 `ext4-formatted`、`ext4-populated`、`ext4-resized` 保留用于分阶段诊断。Ubuntu NoCloud seed 按 instance-id 首次执行，重新运行同一阶段时需准备新的唯一 instance-id seed；单纯重复启动不会重跑已完成的 cloud-init 阶段，必须检查新日志里的 PASS，不能沿用旧日志。重建整轮环境可在两台 VM 停止后清理 `.vm/ubuntu/` 再按 init 流程准备。

完整阶段哈希、界面结果和限制见项目 README 与 `.vm/ext4-status.json`。宿主未执行原始设备 I/O 或磁盘管理操作。

### 原生 FAT 验收

`DiskStudio-Native-FAT.exe` 只在 A/B 各新建 2 MiB、128 MiB、512 MiB 分区，分别验证 FAT12、FAT16、FAT32 的默认及自选簇大小，每次核对 512 KiB 文件内容和全部旧分区布局。客体 `C:\ProgramData\DiskStudio-native-fat-checks.txt` 为单次执行标记与记录，失败后禁止自动重跑。大于 32 GiB 的 FAT32 目前只有合成卷证据，不能沿用小容量测试的验收结论。

### 盘符与卷标验收

`DiskStudio-Metadata-Checks.exe` 只在隔离 Windows VM 中启动；先校验 UUID、精确三盘身份和容量，在 A/B 各新建三个 64 MiB NTFS/exFAT/FAT32 分区。对每个分区执行修改卷标、清除卷标、恢复卷标、更换盘符、移除盘符、恢复盘符，共 36 次元数据变更。每步核对卷 GUID、文件系统、盘符/卷标和 64 KiB 文件内容，检查全部旧分区及 OS 布局不变；另要求被 OS 占用的 C 盘符在预览阶段即被拒绝。无盘符时通过已验证的新测试卷 GUID 路径核对文件，不访问客体 OS 数据。单次执行标记和逐步日志位于客体 `C:\ProgramData\DiskStudio-metadata-checks.txt`，失败后禁止自动重跑。测试增加每盘 192 MiB 占用，与原生 FAT 和新 Ext2/3 夹具合计仍应预先核对剩余空间。该程序目前只构建，未执行，结果不得视为已验收。

### 从所选备份还原点还原

以相同桌面摘要运行 `scripts/vm/build-guest-checks.ps1 -RetainedRestore`，选入 `Retained-Restore-Prepare.exe`、`Retained-Restore-After-Exact.exe` 和 `Retained-Restore-Verify.exe` 制作只读工具光盘。通过默认 512 字节来宾预检后运行 Prepare。它读取 A/20 和 A/21 的已知 FAT12 与 exFAT 夹具，只在核对过的 A/2 创建备份文件。打开 B 上两个经过全零核验的范围作为专用接收分区。

首轮准备因 GPT 条目槽位核对停止。该次现场可用 `Retained-Restore-Assess.exe` 复查。它在内存中排除已知测试条目，复现上轮记录的完整 GPT 摘要，再核对原始源、已准备的测试文件、目标范围和相邻扇区。只有通过这些核对才在 A/2 保存供还原测试使用的基线。

在「备份与克隆 → 浏览备份」打开 `Exact.dsbackup`，从当前点选择分区，按原尺寸还原到报告中的 1 MiB 接收分区。After-Exact 独立核对源、备份、两盘 GPT、还原输出和第二个目标的完整摘要，然后联机 B。打开 `Latest.dsbackup`，按提示选 `Earlier.dsbackup` 作为依赖。切换到最早完整备份，选择分区并调整容量还原到 64 MiB 目标。Verify 用原始引导卷序列号区分两个还原点，验证独立提取的样本、整个目标摘要、源分区、备份文件、邻接扇区和两盘 GPT。B 最终仍脱机。

### FAT 按文件克隆验收

用 `scripts/vm/build-guest-checks.ps1 -FileClone` 和同一正式程序生成包含 `File-Clone-Prepare.exe`、`File-Clone-Verify.exe` 的只读工具光盘。通过 512 字节来宾预检后执行 Prepare，读取 A 上 128 MiB `DSFAT16` 的原始文件树和已知样本，在核对过的 B 空白范围创建 2 MiB 接收分区。记录完整源摘要、两盘 GPT、目标相邻扇区和客体系统盘布局。

在正式程序中选报告里的源和目标，使用「按文件克隆」。确认摘要中的 FAT16 到 FAT12、文件数、目录数和目标串号。完成后 Verify 对照每个活动文件的 SHA-256、文件树名称和元数据，并核对整个目标摘要、FAT12 几何、源数据、相邻扇区、两盘 GPT 和客体系统盘布局。B 保持脱机。

exFAT 用例使用 `-FileCloneExfat` 生成 `File-Clone-Exfat-Prepare.exe` 与 `File-Clone-Exfat-Verify.exe`。源为 A/21 的 96 MiB `DSCAPEX`。目标为 B 上在空白范围准备的新 16 MiB `EXCLONE` 分区，使用常规 FAT16 格式化准备。原生预览应明确 exFAT 到 exFAT，验收另核对主备引导区域和校验、目标起始 LBA、容量与卷标。两个用例使用不同的目标和验收基线。

原始准备在 16 MiB exFAT 格式化预览停下。该次现场使用 `File-Clone-Exfat-Continue.exe`。在内存中核对上轮完整 GPT 摘要，验证此次新增分区的唯一性和全部内容仍为空，再仅在这个分区执行 FAT16 准备。继续使用哈希相同的桌面构建。

NTFS 用例使用 `-FileCloneNtfs`。Prepare 在核对两盘布局和专用空白范围后，用 Windows 格式化 A 上的 96 MiB `DSNTCOPY`，写入中文目录、空目录、命名数据流和硬链接，再准备 B 上 48 MiB 的 `NTDEST`。已有完整源样本且 B 仍无目标时，可用 `File-Clone-Ntfs-Assess.exe`。它复核之前的 GPT 和来宾系统盘布局，锁定源卷并核对样本，然后准备 B。已有这两个夹具且基线尚未生成时，可用 `File-Clone-Ntfs-Confirm-Prepared.exe`。它检查两项分区新增之外的原有布局，核对 B 的 FAT16 目录，重新核对源文件清单并记录克隆前基线。正式程序应显示 NTFS 到 NTFS。Verify 复核整个目标摘要、主备引导区、文件和数据流、硬链接、时间、属性和安全描述符，也核对源摘要、两盘 GPT、相邻扇区和客体系统盘布局。B 保持脱机。

`File-Clone-Ntfs-Source-Lock.exe` 用于定位源分区的整区摘要差异。它从同一个克隆见证核对 A/B 夹具和布局，两次读取源分区作对照，然后分别读取锁定中和解锁后的内容。报告按 NTFS 元数据数据段和 MFT 记录列出发生变化的扇区。源目录清单、保护范围以及脱机 B 盘的整区摘要均在实验后复核。

4Kn 用例使用独立的 4096 字节 A/B 测试盘。`File-Clone-Ntfs-4Kn-Prepare.exe` 在确认空白范围后准备 `DSNT4KCOPY` 和 `NT4KDEST`。`File-Clone-Ntfs-4Kn-Baseline.exe` 核对稳定的两次源读取，在 A 的既有测试数据卷保存压缩基线及全分区摘要。然后从正式界面执行一次按文件克隆。`File-Clone-Ntfs-4Kn-Verify.exe` 逐字节对照源分区。差异须落在两次读取共同确认的 NTFS 或 TxF 日志范围，或通过 MFT 与目录索引记录的跨扇区写入保护序列检查。验证每个序列值、恢复所保护的字节并去除日志序列号后，逐字节核对记录。`$Extend/$RmMetadata/$TxfLog` 常驻或非常驻目录索引中，仅已核验的日志文件长名和 DOS 短名条目允许修改时间及 MFT 更改时间前移。名称、引用、创建时间、访问时间和其余字节必须一致。后验还复核源文件清单、目标全分区 SHA-256、GPT、相邻扇区和客体系统盘布局。B 保持脱机。

当 4Kn 作业在目标写入前因源卷状态变化停止时，`File-Clone-Ntfs-4Kn-Inspect-Source.exe` 可逐项对照源记录。`File-Clone-Ntfs-4Kn-Settled-Baseline.exe` 核对原始基线和目标内容，在只操作源卷锁的前提下检查文件清单及锁定中和解锁后的字节，另外保存一份新的基线。然后在正式程序中重新预览。若启动新作业，用 `File-Clone-Ntfs-4Kn-Verify-Settled.exe` 校验它对应的基线。

短名和扩展属性用例使用 `-FileCloneNtfsMetadata`。`File-Clone-Ntfs-4Kn-Metadata-Prepare.exe` 只用新空白范围准备 `DSNT4KMETA` 和 `NT4KMETA`。源 NTFS 格式化开启短名，用 Windows 创建长文件名并保存 4199 字节的扩展属性。`File-Clone-Ntfs-4Kn-Metadata-Baseline.exe` 在锁卷前后独立确认范围及文件清单，要求 Windows 生成独立短名并核对属性名和值，再保存全源分区字节基线。正式界面执行克隆后，`File-Clone-Ntfs-4Kn-Metadata-Verify.exe` 用原基线独立核对短名、硬链接、扩展属性、文件内容、目标完整 SHA-256、GPT 和源分区字节。夹具通过新目录的一次性记录保护原有验收盘内容。[Windows 扩展属性记录格式](https://learn.microsoft.com/en-us/windows-hardware/drivers/ddi/wdm/ns-wdm-_file_full_ea_information)

首轮在源卷格式化后遇到 R: 已被占用，B 的写入计数保持为零。`-FileCloneNtfsMetadataContinue` 绑定已核对的正式程序并打包续验入口。来宾先核对既有 A 分区、原 GPT 和边缘、源卷文件清单及 B 的原空白范围。修正程序识别了事务日志的 DOS 短名后，续验发现来宾盘符已经用完。此后的工具用卷 GUID 打开源卷根目录，核对保留句柄与原分区身份，再从既有 A 分区填样并首次建立 B 的目标。卷 GUID 路径和填样操作仅用于来宾夹具。现场结果见 `.vm/ntfs-clone-metadata-status.json`。 [Windows 卷 GUID 路径](https://learn.microsoft.com/en-us/windows/win32/fileio/naming-a-volume)

稀疏与压缩流用例使用 `-FileCloneNtfsStorage`。将三个 `File-Clone-Ntfs-4Kn-Storage` 入口和匹配的正式程序放入同一只读光盘。Prepare 核对隔离范围，在新建的 `DSNT4KSTOR` 上通过 Windows 生成并回读稀疏文件、LZNT1 文件与命名流，在 B 创建 `NT4KSTOR`。Baseline 核对源簇分配与摘要，并保存源分区和布局基线。正式界面执行 96 MiB 到 48 MiB 的按文件克隆。Verify 核对普通文件与命名流、稀疏分配、目录压缩标志、两盘 GPT 和目标整区 SHA-256，逐扇区审核源变化。结果需确认 B 保持脱机。

4Kn 复验使用 `-FileCloneNtfsStorageAudit`，光盘保留同一桌面程序，仅选择 `File-Clone-Ntfs-4Kn-Storage-Audit.exe` 复查已有范围。对已核验日志条目，只有缓存长度由零更新为同一文件未命名数据流的实际长度并通过分配核对时，源审计才允许该字段发生变化。A/60 到 B/10 的目标摘要一致，23 个源盘变化扇区均完成归因，两轮独立审核期间 A 和 B 无新增写入。结果见 `.vm/ntfs-clone-storage-status.json`。

对象 ID 和链接用例使用 `-FileCloneNtfsLinks`。在只读光盘选择三个 `File-Clone-Ntfs-4Kn-Links-*.exe`，绑定同一桌面程序。Prepare 在新建的 `DSNT4KLINK` 生成文件和目录对象 ID，原生回读全部 64 字节，并创建相对符号链接及指向测试卷内目录的联接。Baseline 保存源分区字节和两盘布局。正式界面从 `DSNT4KLINK` 向 `NT4KLINK` 执行按文件克隆。Verify 读取 B 的脱机范围，按活动文件记录核对对象和重解析索引，比较扩展字段、原始链接数据、硬链接身份和完整分区摘要。

`-FileCloneNtfsLinksInspect` 绑定首轮构建，只读核对 A/61 的原生目录联接记录。`-FileCloneNtfsLinksContinue` 绑定修正版，核对已准备的 A/61 和 B/11 后才保存基线。原生克隆一次完成。用 `-FileCloneNtfsLinksAudit` 复验同一目标和源基线，根目录索引的两个时间缓存须与同一活动记录的实际时间相符，其余字节逐一比较。复验完成全部 23 个差异扇区归因。记录见 `.vm/ntfs-clone-links-status.json`。

卷内 WOF 可在来宾通过 `-FileCloneNtfsWof` 用 Windows 创建。来宾无法生成 WOF 文件时，将通过独立解压验证的 `Ntfs-Wof-Source.raw` 放入 `.vm/payload`，用 `-FileCloneNtfsWofImage` 生成三个 `File-Clone-Ntfs-4Kn-Wof-Image-*.exe`，再将三个入口放入同一只读光盘。Prepare 校验光盘摘要，并在锁定、脱机和二次确认目标为零后写入 A 的新数据分区。该分区的 NTFS 卷名为 `DISKCLONE`，B 的 48 MiB 目标卷名为 `NT4KWOFIMG`。Baseline 核对四种压缩流、普通 ADS、稳定源字节和布局。用正式界面执行按文件克隆后，Verify 从脱机 B 校验四种压缩数据、重解析记录、源清单、GPT 和完整目标 SHA-256。

初次准备在镜像写入前停止时，`-FileCloneNtfsWofImageContinue` 仅在原 A 分区及 B 计划范围仍全零、布局和相邻扇区完整时继续。镜像写入并读回完成、Windows 报告联机成功而卷关联尚未通过时，`-FileCloneNtfsWofImageAfterOnline` 重新核对 A 的当前卷身份、四种压缩流及 B 的空白范围，随后准备 B，对应的两个 Settled 入口保存基线和执行后验。

EFS 用例使用 `-FileCloneNtfsEfs`。Prepare 在新建的 `DSNT4KEFS` 上调用 Windows EFS API 加密测试文件和目录，在同一账户下核对逻辑内容，再准备 B 上的 48 MiB `NT4KEFS`。Baseline 核对加密标志、命名流、密文和 `$EFS` 元数据，记录稳定源分区及布局。使用正式界面执行按文件克隆。Verify 通过脱机读回比较完整密文、元数据、目标分区摘要和布局。

### FAT 与 exFAT 不同容量克隆验收

用匹配的正式程序运行 `scripts/vm/build-guest-checks.ps1 -ResizeClone`，工具光盘选择 `Resize-Clone-Prepare.exe`、`Resize-Clone-After-Growth.exe` 和 `Resize-Clone-Verify.exe`。通过默认 512 字节来宾预检后，Prepare 核对 A/20 和 A/21 的已有夹具，对 B 的两个指定空闲范围逐字节确认无数据，再创建专用 GPT 目标。准备结束时报告目标分区编号。

首轮夹具的 exFAT 卷标在格式化预览被拒绝。对应续验使用 `-ResizeCloneContinue`，绑定同一桌面摘要，用原有准备标记、两个目标几何、首个 FAT12 卷标及第二目标全零内容核对现场，只格式化第二目标，然后记录完整基线。

在原生界面先将 A/20 FAT12 由 1 MiB 克隆至 2 MiB 目标。After-Growth 在 B 脱机时核验引导字段、样本内容和目标 SHA-256，确认另一个目标未改动，再联机 B。刷新后将 A/21 exFAT 由 96 MiB 克隆至 64 MiB 目标。最终 Verify 核对两个目标的起始 LBA、卷容量、样本内容和完整 SHA-256，并复查两个完整源分区、两盘 GPT、目标外邻接扇区和来宾系统盘布局。结束时 B 应保持脱机。

### Btrfs 变容量克隆验收

用匹配的正式程序运行 `scripts/vm/build-btrfs.ps1 -CapacityClone`。以 `make-tools-iso.ps1 -Live -BtrfsCloneFixture` 选择 `Btrfs-Lab-4Kn-Clone-Prepare.exe`、`Btrfs-Lab-4Kn-Clone-Inspect-Shrink.exe` 和 `Btrfs-Lab-4Kn-Clone-Inspect-Grow.exe`。运行 `node scripts/vm/lab.mjs start --4kn --btrfs-clone` 和 `preflight`，核对独立 A/B 镜像和两块控制器。

Prepare 只接受两块空 RAW 盘和匹配 SHA-256 的来宾载荷。它在 A 建立 384 MiB 的 Linux GPT 分区并写入已知镜像，在 B 准备 192 MiB、448 MiB 两个 Linux GPT 目标。A 保持脱机。正式界面选择 A 分区，用「调整容量克隆」先写入 192 MiB 的 B 分区。预览应报告 80 MiB 条带搬移。通过 Inspect-Shrink 后，选择 B 上的 448 MiB 分区验收扩容，B 全程保持脱机。后验独立核对两次目标 SHA-256、51 个文件、卷 UUID、完整源分区以及两盘 GPT 和来宾系统盘布局。

夹具写入后可用 `Btrfs-Lab-4Kn-Clone-Resume.exe` 只读复核 A 的源镜像、B 的两个空白目标及两盘布局，为匹配的桌面载荷建立基线。预览遇到保护状态拦截时，`Btrfs-Lab-4Kn-Clone-Diagnose.exe` 从该基线定位分区，报告来宾的磁盘脱机状态、分区保护字段、卷关联和策略判断。

### Btrfs 备份还原验收

用 `node scripts/vm/lab.mjs start --4kn --btrfs-restore` 启动独立 A/B 测试盘。以匹配的正式程序运行 `build-btrfs.ps1 -Restore`，用 `make-tools-iso.ps1 -Live -BtrfsCloneFixture` 选择 `Btrfs-Restore-4Kn-Prepare.exe`、`Btrfs-Restore-4Kn-Resume.exe` 和 `Btrfs-Restore-4Kn-Verify.exe`。通过 QMP 预检后运行 Prepare。A/B 为空 RAW 时，Prepare 在 A 的 NTFS 卷创建按已用空间保存的 Btrfs 完整备份和增量备份，在 B 准备 384、192、448 MiB 三个 Linux 数据分区。用「浏览备份」打开 `Latest.dsbackup`，按提示选择 `Earlier.dsbackup`。从旧点依次原尺寸还原到 384 MiB 并缩容到 192 MiB，从最新点扩容到 448 MiB。B 在第一次还原后保持脱机。Verify 独立核对三个目标完整摘要、51 个文件、卷 UUID、版本标记、相邻扇区、备份文件、两盘 GPT 和来宾系统盘布局。已完整准备的夹具可用匹配载荷的 Resume，只在三个目标均为空白时重建基线。

### 加密整盘备份还原验收

用 `node scripts/vm/lab.mjs start --4kn --archive-restore` 启动独立 A/B 测试盘。正式构建完成后运行 `build-guest-checks.ps1 -ArchiveRestore`。用 `make-tools-iso.ps1 -Live` 选择 `Archive-Restore-4Kn-Prepare.exe`、`Archive-Restore-4Kn-Verify-Partition.exe` 和 `Archive-Restore-4Kn-Verify-Disk.exe`。通过预检后运行 Prepare。入口复核来宾身份、A/B 的完整序列号、物理和逻辑扇区以及桌面程序摘要，只在两盘均为空 RAW 时开始。它在 A 的 NTFS 数据卷准备两层加密整盘归档，在 B 创建 8 MiB 普通数据目标。增量点的主 GPT 校验和故意损坏。

在「浏览备份」打开 `Latest.dsbackup` 及其 `Earlier.dsbackup` 依赖，使用准备驱动中定义的两个测试密码。最新点应显示分区表校验提示，并禁用目录读取及分区还原。切换到最早完整点，选择备份内的 8 MiB 分区并原尺寸还原到 B 的测试分区。先运行 Verify-Partition 核对目标、相邻扇区和原有布局。再选择最新点，确认整盘还原警告及 B 的完整序列号，执行整盘还原。Verify-Disk 核对 B 的全盘 SHA-256、原样保留的分区表与分区内容，并核对两份归档、A 的 GPT、来宾系统盘布局和 B 的脱机状态。

首轮准备因合成 GPT 的来源指纹格式错误而停止，B 的 QMP 写入计数为零。该情况使用只读光盘上的 `Archive-Restore-4Kn-Continue.exe`。它要求 B 仍为空 RAW，A 上的预期 GPT 和 NTFS 卷仍完整，且此前目录只包含零字节的 `Earlier.dsbackup`。它保留该文件并把成功归档写入新的 `ArchiveSet` 子目录，A 的分区准备无需再执行。

构建 `6fe9c432…` 的隔离验收通过。旧点的 8 MiB 分区和最新点的 16 GiB 整盘均完成独立 SHA-256 回读，B 保持脱机。QMP 记录两次写入各自恰好覆盖所确认的目标容量。首次整盘作业在写入前校验期间遇到来宾关机。重新启动后先用 Verify-Partition 核对 B 仍是上一次单分区写入后的状态，再重新打开归档和确认整盘预览。截图、摘要和 I/O 计数见 `.vm/archive-restore-status.json`。

### 按文件与变容量还原验收

运行 `node scripts/vm/lab.mjs start --4kn --capacity-restore`，加载独立 A/B 测试盘。完成正式构建后，运行 `build-guest-checks.ps1 -CapacityRestore`，用 `make-tools-iso.ps1 -Live` 选择 `Capacity-Restore-4Kn-Prepare.exe`、`Capacity-Restore-4Kn-After-Ntfs.exe`、`Capacity-Restore-4Kn-After-Fat32.exe` 和 `Capacity-Restore-4Kn-Verify.exe`。通过预检后运行 Prepare。它验证两盘为空 RAW，在 A 的 NTFS 数据卷写入三份备份，在 B 准备物理 NTFS 源分区和三个目标分区。NTFS 归档由锁定的 B 分区写到 A。FAT32 和 exFAT 归档由合成卷生成。

在「浏览备份」依次打开 Prepare 显示的三份归档。NTFS 96→48 MiB 和 exFAT 8→6 MiB 选择「按文件还原」，FAT32 268→260 MiB 选择「调整容量还原」。确认窗应显示 FAT32 搬移一个簇。核对 B 的完整序列号、分区号和容量后逐项确认。每次作业结束时 B 脱机，Windows 可能在这一状态下省略目标保护属性。NTFS 完成后执行 After-Ntfs，FAT32 完成后执行 After-Fat32。两者先在脱机状态独立回读整个目标、核对源归档、相邻扇区、A/B GPT 和系统盘布局，再分别单次联机 B，为下一项准备。刷新正式程序磁盘列表后再预览下一项。exFAT 写完后 Verify 核对三项样本及还原容量，汇总各次脱机 SHA-256，并保持 B 脱机。

首轮准备在 48 MiB 目标的 NTFS 格式化预览处停止。遇到该已核对的状态时，用 `Capacity-Restore-4Kn-Continue.exe` 核对三个源归档、B 的既有布局和空白分区。它以 FAT16 格式化已经创建的 48 MiB 目标，并从尚未执行的后续步骤继续。

正式构建 `73d7e744…` 通过 828 项项目检查、501 项来宾工具合成测试和载荷 SHA-256 核对。三项作业均通过原生界面验收，独立脱机回读的完整摘要与界面一致。FAT32 的活动簇由 68443 搬到 3。最后一次独立回读期间 A/B 无新增写入，B 保持脱机。记录见 `.vm/capacity-restore-status.json`。

后续 FAT 变体验收使用 `node scripts/vm/lab.mjs start --4kn --fat-restore`。用 `build-guest-checks.ps1 -FatRestore` 和相同桌面程序生成只读工具光盘，选择 `FAT-Restore-4Kn-Prepare.exe` 与 `FAT-Restore-4Kn-Verify.exe`。预检后 Prepare 只在两块专用空 RAW 盘上建立夹具。先在正式界面打开 `Geometry.dsbackup`。其归档记录为 4Kn，里面的 GPT 使用 512 字节扇区。应能校验归档字节并显示扇区大小冲突，界面仅提供整盘还原入口。本用例只检查入口，不确认写入。

依 Prepare 输出的顺序验收 FAT12 8→4 MiB、FAT16 48→24 MiB、FAT32 268→8 MiB 按文件还原，以及 FAT16 48→32 MiB 和 exFAT 8→6 MiB 缩容还原。FAT32 按文件还原到 FAT12。两项缩容预览应各显示搬移一个簇。每次写入后运行 Verify，核对文件、目标格式与容量、卷标识、簇搬移和完整 SHA-256；同时检查源归档、GPT、相邻扇区和来宾系统盘布局。Verify 只在这一次离线读回通过后保存记录，前四次会为下一项单次联机 B，最后一次保持 B 脱机。

正式构建 `f86db157…` 通过全部五项原生还原验收及扇区大小冲突的界面检查。FAT16 将簇 12272 搬到 2，exFAT 将簇 2009 搬到 4。五个目标的独立脱机 SHA-256 均与界面一致，两盘 GPT 和相邻扇区通过复核。记录见 `.vm/fat-restore-status.json`。

### Ext 分区还原和同容量克隆验收

使用 `node scripts/vm/lab.mjs start --ext-copy` 启动专用 512 字节 A/B 盘。用 `build-guest-checks.ps1 -ExtCopy` 和匹配的正式程序生成只读光盘，选入 `Ext-Copy-Prepare.exe`、`Ext-Copy-Verify-Restores.exe` 和 `Ext-Copy-Verify-Clones.exe`。通过预检后运行 Prepare，它只在两盘均为空 RAW 时准备数据。在 A 的 NTFS 数据卷保存 Ext2、Ext3 和 Ext4 按已用空间归档，在 B 建立三个普通 GPT Linux 目标，并在 A 新建两个 64 MiB Linux 目标。

先在「浏览备份」打开三个归档，依报告分别原尺寸还原到 B。Verify-Restores 在 B 脱机期间核对三个目标的完整 SHA-256、样本文件、UUID、两盘 GPT、相邻扇区、A 上的三个归档和来宾系统盘布局。关闭已打开的归档以释放 A 上的文件句柄。保持 B 脱机，从正式界面的「磁盘与分区克隆」选择已还原的 B Ext4，分别按已用空间和逐扇区克隆到 A 的两处分区。Verify-Clones 核对两盘均脱机、全部五个分区的文件与字节、两种克隆摘要、两盘 GPT、相邻扇区和系统盘布局。QMP 记录目标盘总写入量，并与作业报告的复制量对照。

正式构建 `9556a106…` 通过原生验收。Ext2 和 Ext3 各还原 96 MiB，Ext4 还原 64 MiB。Ext4 按已用空间克隆复制 29,628,416 字节，跳过 37,480,448 字节。逐扇区克隆复制 64 MiB。按已用空间作业期间 A 全盘计数比复制量多 184,320 字节，来源尚未独立确认。三个还原作业及逐扇区克隆的目标写入计数增量与计划容量一致。最终独立读回期间 A/B 均无新增写入，两盘保持脱机。记录见 `.vm/ext-copy-status.json`。

### Btrfs 同容量克隆验收

在 Windows 与 Ubuntu 测试虚拟机均关闭时，将已验收的 `test-a-btrfs-restore-4kn.qcow2` 和 `test-b-btrfs-restore-4kn.qcow2` 分别复制为 `.vm/windows/test-a-btrfs-exact-clone-4kn.qcow2` 和 `.vm/windows/test-b-btrfs-exact-clone-4kn.qcow2`，核对复制前后的 SHA-256。运行 `node scripts/vm/lab.mjs start --4kn --btrfs-exact-clone` 和 `preflight`。用 `build-guest-checks.ps1 -BtrfsExactClone` 绑定已核对 SHA-256 的正式程序。只读工具光盘选入 `Btrfs-Exact-Clone-4Kn-Prepare.exe` 和 `Btrfs-Exact-Clone-4Kn-Verify.exe`。

准备阶段需要读取 A 上的历史归档。若来宾启动后 A 脱机，先在正式界面的「更改设备状态」预览中核对 A 的完整序列号、16 GiB 容量和两个原有分区，然后单次确认联机。B 保持脱机。

Prepare 复核此前的 Btrfs 还原验收记录及 B 上的三个已还原分区。在 A 的两处未分配空间建立 384 MiB 的普通 GPT Linux 分区，确认两个范围全零后，把一个 4 KiB 标记写入第一目标中对应源卷已核对的空闲扇区，保持 A/B 脱机。按 Prepare 输出选择源 B/2 和两个目标。使用「按已用空间」时核对预计复制和跳过的范围大小，使用「逐扇区」时核对整分区容量。Verify 复核两个完整目标的 SHA-256、已复制范围的摘要、51 个样本文件、卷 UUID、空闲扇区标记、两盘 GPT、相邻扇区和来宾系统盘布局。用 QMP 分别记录两个克隆和独立读回期间的 A/B 写入计数。

正式构建 `a10c64a1…` 通过 829 项项目检查、503 项来宾工具合成测试、载荷一致性核对及两项原生克隆验收。按已用空间克隆复制 112,365,568 字节，跳过 290,287,616 字节并保留 4 KiB 标记。逐扇区克隆复制完整 384 MiB。QMP 记录的 A 写入增量分别与两项计划一致，B 无新增写入。独立回读通过，两盘均保持脱机，后验期间未新增 A/B 写入。记录见 `.vm/btrfs-exact-clone-status.json`。

### 按空闲簇和扇区对齐的签名恢复验收

使用 `node scripts/vm/lab.mjs start --4kn --signature-scope` 和 `preflight`。用 `build-guest-checks.ps1 -SignatureScope` 绑定正式程序的 SHA-256，工具光盘选入 `Signature-Scope-4Kn-Prepare.exe` 和 `Signature-Scope-4Kn-Verify.exe`。Prepare 只在两块专用盘均为空 RAW 时准备夹具。A 的 NTFS 卷提供输出目录，B 的专用 8 MiB 分区包含一个已分配的对齐 PDF，一个空闲区未对齐 PDF 和一个空闲区对齐 PDF，准备结束后 B 保持脱机。

在正式界面选择来宾报告中的 B 分区，启用 PDF 一种类型。全范围预期 3 个候选，开启扇区对齐预期 2 个，同时开启仅空闲簇预期 1 个。将最终进度保存为报告目录内的 `Scope.dsrecover`，加载后预览并平面导出余下的 PDF。Verify 核对完整源分区、相邻扇区、两盘 GPT、来宾系统盘布局、保存的进度选项及候选、导出的内容和清单 SHA-256。用 QMP 记录扫描、保存加载、导出与核对期间的 A/B 写入计数。

正式构建 `c6f23377…` 的三个原生扫描、进度保存与加载、46 字节候选预览和导出均通过。独立工具第一次将原候选名作为平面输出名，未计入编号前缀。更正独立工具后，只读复核原有输出及清单通过。正式桌面程序摘要一致，源 B 从完成夹具准备到后验均无新增写入，整个 8 MiB 源分区逐字节一致。英文载入结果仍有一条中文提示。记录见 `.vm/signature-scope-status.json`。

7z 与 SQLite 使用独立的 `node scripts/vm/lab.mjs start --4kn --signature-formats` 测试盘和 `build-guest-checks.ps1 -SignatureFormats` 生成的两项入口。普通文件样本来自 Windows `tar.exe` 的 7zip 写入及 `winsqlite3.dll` 建立的 512 字节页数据库。建样时分别解出文本，以及运行 SQLite `PRAGMA integrity_check` 并回读行。Prepare 将两个样本放进脱机 B 的两个对齐空闲簇，保留原 PDF 对照。仅选择 7z 和 SQLite，同时开启空闲簇与扇区对齐。检查两个候选的大小、保存进度、加载提示和两个十六进制预览。将两份候选平面导出到 A 的同一测试目录。Verify 核对完整源、两个导出、清单 SHA-256、相邻扇区、GPT 和来宾系统盘布局。

正式构建 `f129a432…` 通过上述验收。两个导出的原始大小为 184 字节与 1,024 字节，清单摘要均与独立样本相符。B 保持脱机，准备完成后无新增写入。英文界面的进度加载状态和提示均显示英文。记录见 `.vm/signature-formats-status.json`。

### 整盘按文件克隆验收

混合克隆使用 `node scripts/vm/lab.mjs start --4kn --mixed-clone` 的独立 A/B 盘。以匹配的正式程序运行 `build-btrfs.ps1 -MixedClone`，再通过 `make-tools-iso.ps1 -Live -BtrfsCloneFixture` 选择 `Mixed-File-Clone-4Kn-Prepare.exe` 和 `Mixed-File-Clone-4Kn-Verify.exe`。通过 QMP 预检后运行 Prepare。两块盘为空 RAW 时，Prepare 在 A 生成 384 MiB Btrfs、32 MiB 不透明数据和 96 MiB NTFS 分区，保留 A 联机及 B 空白。整盘按文件克隆的原生预览应显示 Btrfs 384→192 MiB 并搬移 80 MiB 条带、32 MiB 分区原位置逐扇区复制及 NTFS 96→48 MiB 按文件重建。独立 Verify 核对两盘布局、Btrfs 的 51 个文件和卷 UUID、NTFS 样本、32 MiB 全范围、清零区和整盘 SHA-256。对已经完整准备且 B 仍为空 RAW 的夹具，可用匹配载荷中的 `Mixed-File-Clone-4Kn-Resume.exe` 复核源内容并建立验收基线。

使用 `node scripts/vm/lab.mjs start --4kn --whole-clone` 启动专用 4Kn A/B 盘，执行 `preflight` 核对来宾身份、控制器和文件后端。将同一份已核对 SHA-256 的桌面程序放在 `.vm/artifacts/DiskStudio.exe`，运行 `scripts/vm/build-guest-checks.ps1 -WholeFileClone`。工具光盘选择 `Whole-File-Clone-4Kn-Prepare.exe` 和 `Whole-File-Clone-4Kn-Verify.exe`。

Prepare 在来宾中重验程序 SHA-256、虚拟机 UUID、完整 NVMe 序列号及扇区大小。两盘均为空 RAW 时才初始化并在 A 建立三个数据分区，写入并回读 NTFS、FAT16、exFAT 样本，核对 B 仍为空 RAW。正式界面选择 A 整盘按文件克隆到 B，分别将三个分区设为 48、64、80 MiB。Verify 保持 B 脱机，核对原始 GPT、Windows 原生布局、新标识、容量、样本和零填充，并独立计算整盘 SHA-256，与正式界面结果比较。核对克隆前后 A/B 的 QMP 读写计数，记录任何变化。

首轮准备在写入前停止。全新盘的 Storage Provider 样式为 RAW，原生接口返回零条目的 48 字节 MBR 头。修正的写入检查要求该 MBR 标识为零，并核对磁盘首尾实际分区表扇区全部为零。

修正后的首次准备在 NTFS 文件写回后未能立即从原始目录读到样本。`Whole-File-Clone-4Kn-Resume.exe` 仅接受 A 上已存在的指定 NTFS 分区及空 RAW B。锁定并核对 NTFS 源卷后先读回文件，再创建 FAT16 和 exFAT 分区。三个源样本均在锁卷下独立读取，保留已写好的 NTFS 文件。

首次原生预览识别出 Windows 创建的 Microsoft 保留分区约为 15.98 MiB。整盘克隆按实际扇区长度重建保留区，并核对数据分区与源盘所有卷关联的数量完全一致。用匹配新构建的 `Whole-File-Clone-4Kn-Rebind.exe` 核对既有三份源样本、GPT、系统盘布局与空 RAW B，保存本次验收基线。

正式构建 `36207dc5…` 在原生界面完成了 A 到空 RAW B 的整盘按文件克隆。独立读回核对 NTFS 96→48 MiB、FAT16 80→64 MiB、exFAT 96→80 MiB、MSR 精确长度、重建的 GPT 标识与双副本，以及未使用范围全零。来宾系统盘布局和 A 的 GPT 与样本一致，B 保持脱机。整盘 SHA-256 与界面一致。QMP 在克隆阶段记录 A 的 36 次写入共 212,992 字节，独立后验阶段 A、B 均无新增写入。构建、截图、后验摘要和 I/O 计数见 `.vm/whole-disk-file-clone-status.json`。

### BitLocker 引导识别验收

用匹配的正式程序运行 `scripts/vm/build-guest-checks.ps1 -BitLockerMarker`，工具光盘选择 `BitLocker-Marker-Prepare.exe` 和 `BitLocker-Marker-Verify.exe`。通过 512 字节 Windows VM 预检后，在来宾中运行 Prepare。该工具验证 A/B 盘身份与正式程序摘要，在已核对的 A/2 数据卷创建带合成 BitLocker 标记的分区镜像，以及一个普通 NTFS 对照镜像。

在正式界面用「备份与克隆 → 浏览镜像」分别以分区镜像打开两份文件。合成标记应提示先在 Windows 解锁，且目录为空。对照镜像应能列出文件并预览 `file-with-12345`。Verify 逐字节复核两个源镜像，并核对 A/B 的 GPT 和客体系统盘的分区元数据。此批验收覆盖引导标记识别和普通 NTFS 回归。真实加密卷需在具备 BitLocker 组件的来宾另行验收。

### 虚拟 RAID 镜像验收

`node scripts/vm/build-virtual-raid.mjs` 从 `.vm/virtual-raid-original.raw`（独立核对过的 48 MiB Ext4 LVM 逻辑卷）生成三份 25 MiB RAW 成员。虚拟 RAID5 使用左对称布局、64 KiB 条带，成员数据从 1 MiB 偏移开始。生成脚本独立计算校验条带，并生成逻辑内容和三个成员的 SHA-256 清单。

确保存档中的 `DiskStudio.exe` 与当前桌面构建一致。运行 `scripts/vm/build-virtual-raid.ps1`，再用 `make-tools-iso.ps1 -Live -GuestTools @('Virtual-Raid-4Kn-Prepare.exe','Virtual-Raid-4Kn-Inspect.exe')` 准备工具光盘。通过 4Kn 来宾预检后，在来宾中启动 Prepare。它核对正式程序与光盘成员的摘要，只在确认身份的 A/B 数据卷上创建源文件和输出目录。通过正式界面选齐成员，按文件名编号顺序组装，选择分区范围和 512 B 逻辑扇区。每个成员从字节 1048576 开始，长度为 25165824 字节。保留 `nested` 目录并跨盘导出。Inspect 独立核对目录、文件摘要、清单中的 RAID 参数、成员文件摘要和原有布局。Prepare 使用单次标记，遇到失败先核对当前目录和磁盘身份。

`node scripts/vm/build-virtual-raid.mjs --devices` 生成一份 64 MiB 逻辑阵列，32 MiB Ext4 文件系统从字节 4194304 开始，分区表为空。脚本独立条带化为两份 32 MiB RAID0 成员，并保留一份完整镜像供 RAID1 单成员测试。匹配桌面构建后运行 `build-virtual-raid.ps1 -Devices`，工具光盘选择 `Virtual-Raid-Device-Fresh-Prepare.exe` 和 `Virtual-Raid-Device-Fresh-Inspect.exe`。Prepare 仅在 4Kn 隔离来宾的 A 盘新建三个 Linux 测试分区，确认范围为空才写入，完成后 A 保持脱机。界面按助手显示的分区编号组装 RAID0，按助手显示的绝对偏移组装只选 A 整盘且显式缺一员的 RAID1。两套阵列都用 4096 B 逻辑扇区，在整盘搜索模式核对 LBA 1024 的 Ext4 候选，分别按提示选择 B 盘已有输出目录。Inspect 独立核对两个导出清单与文件摘要、三个完整源成员、A/B GPT 和系统盘布局。

`node scripts/vm/ubuntu.mjs seed-virtual-raid-md6-4kn` 制作 Linux md RAID6 的只读种子。运行 `node scripts/vm/ubuntu.mjs virtual-raid-md6-4kn` 后，Ubuntu 在 tmpfs 上的五个 4Kn loop 成员生成 14 个布局的 16 行条带，并对左对称布局中的 Ext4 文件做独立回读。该阶段仅连接 Ubuntu 系统虚拟盘和种子光盘。Linux 验证完成后，`node scripts/vm/extract-md6.mjs <日志文件名>` 验证并提取条带、五个成员的数据范围和文件 SHA-256。

用匹配的桌面构建运行 `build-virtual-raid.ps1 -Md6` 并在工具光盘中选择 `Virtual-Raid-Md6-Prepare.exe` 和 `Virtual-Raid-Md6-Inspect.exe`。Prepare 把成员镜像复制到已验证的 A 盘数据卷，并在 B 盘数据卷创建导出目录。在 Windows 原生界面选择编号 0、2、4 的成员，按 0、缺失、2、缺失、4 排列，使用 RAID6 左对称布局、4096 B 逻辑扇区和 4 KiB 条带。选择分区镜像，每个成员偏移 0，长度 12582912 字节。保留 `nested` 目录并跨盘导出，Inspect 核对文件摘要、成员顺序和范围、源文件与磁盘布局。

Linux md 元数据验收以 `seed-virtual-raid-md-metadata-4kn` 和 `virtual-raid-md-metadata-4kn` 运行隔离 Ubuntu。该阶段在 tmpfs 生成带完整超级块的 1.0 RAID5、1.1 RAID1 和 1.2 RAID0、RAID4、RAID6、near RAID10。Linux 复核每个阵列的原生容量与逻辑读回，并对 RAID6 内的 Ext4 文件做独立回读。`node scripts/vm/extract-md6.mjs <日志文件名>` 校验并提取所有成员文件及逻辑容量。

用 `build-virtual-raid.ps1 -MdMetadata` 和 `make-tools-iso.ps1 -Live -GuestTools @('Virtual-Raid-Md-Metadata-Prepare.exe','Virtual-Raid-Md-Metadata-Inspect.exe')` 准备 4Kn Windows 来宾。Prepare 核对匹配载荷后，只在已验证的 A/B 数据卷上准备成员和导出目录。在虚拟 RAID 选择 4、0、2 号完整成员文件，选用 4096 B 逻辑扇区，再识别 Linux md 参数。检查填入的布局、偏移、成员顺序和缺失槽位，选择分区镜像并读目录，把 `nested` 跨盘导出。Inspect 独立核对导出文件、超级块身份和事件代次、导出清单、源成员摘要和受保护布局。

不等长 RAID0 验收使用 `seed-virtual-raid-md-zones-4kn` 和 `virtual-raid-md-zones-4kn`。Ubuntu 的三个 4Kn loop 成员依次为 20、6、13 MiB，分别以 original 和 alternate 写入同一份 36 MiB 逻辑内容。Linux 核对整个阵列的 SHA-256，并从 17 MiB 偏移处的 Ext4 独立读回文件。`extract-md6.mjs` 提取完整成员和两个逻辑摘要。

用 `build-virtual-raid.ps1 -MdZones` 和 `make-tools-iso.ps1 -Live -GuestTools @('Virtual-Raid-Md-Zones-Prepare.exe','Virtual-Raid-Md-Zones-Inspect.exe')` 制备 4Kn Windows 来宾光盘。Prepare 在已核对的 A 盘创建三个 Linux 分区。最后一个成员先预读预定的空闲范围，确认空白后才创建分区。写入前在脱机的 A 盘逐字节复核目标为空，完成后 A 保持脱机。选择报告中的三个 A 盘分区和 4096 B 逻辑扇区，识别 Linux md，然后用整盘搜索范围组装。由 LBA 4096 扫描 4352 个扇区，在 LBA 4352 找到 Ext4 并把 `nested` 跨盘导出至报告给出的 B 盘目录。Inspect 核对导出摘要、超级块、物理范围、三个源分区全范围摘要及受保护布局。

Linux md RAID10 验收使用 `seed-virtual-raid-md10-4kn` 和 `virtual-raid-md10-4kn`。Ubuntu 在五个 4Kn loop 成员上分别建立 `n2`、`n3`、`f3` 和 `o2` 阵列，以 64 KiB 条带写入并读回完整逻辑内容。`o2` 的起始位置包含 16 MiB Ext4，Linux 独立读回 `nested/proof.bin`。用 `extract-md6.mjs` 提取原始成员、逻辑容量和 SHA-256。

用 `build-virtual-raid.ps1 -Md10` 和 `make-tools-iso.ps1 -Live -GuestTools @('Virtual-Raid-Md10-Stage.exe','Virtual-Raid-Md10-Verify.exe')` 制备匹配桌面构建的光盘。Stage 将五份 `o2` 成员放在 A 盘现有的、空间充足的数据卷，报告给出目录。在虚拟 RAID 中选取成员 0、2、4，使用 4096 B 逻辑扇区识别 Linux md 参数。确认 offset、两个副本和两个缺失槽位，选择分区镜像，读取 Ext4 目录后将 `nested` 导出至 B 盘。Verify 核对文件与 Linux 的 SHA-256、目录、布局和成员清单、源成员 SHA-256 及受保护分区表。

高级 RAID10 验收使用 `seed-virtual-raid-md10-advanced-4kn` 和 `virtual-raid-md10-advanced-4kn`。Ubuntu 在 tmpfs 上创建四套 4Kn RAID10 基线并停用阵列，确认 loop 成员无持有者后设置混合 near/far、混合 offset、旧式分组或固定混合分组的超级块布局和校验和。Linux 内核重新组装这些临时成员，写入并读回完整逻辑内容。混合 offset 样本另用 `e2fsck` 和 `debugfs` 检查 Ext4 和 `nested/proof.bin`。`extract-md6.mjs` 提取成员及 Linux 摘要。

用 `build-virtual-raid.ps1 -Md10Advanced` 和 `make-tools-iso.ps1 -Live -GuestTools @('Virtual-Raid-Md10-Advanced-Stage.exe','Virtual-Raid-Md10-Advanced-Verify.exe')` 制备只读来宾光盘。Stage 将六份混合 offset 成员写入它核对过的 A 盘现有数据卷。选择成员 0、2、4，使用 4096 B 逻辑扇区识别 Linux md，确认两个近端副本、两个远端副本、offset 模式、跨全部成员和三个缺失槽位。组装后读取 Ext4 目录，把 `nested` 跨盘导出到 Stage 指定的 B 盘卷。Verify 检查导出目录结构、文件摘要、超级块元数据及导出清单、六份源镜像和受保护分区布局。
