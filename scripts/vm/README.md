# QEMU 开发测试环境

本目录仅负责测试环境。DiskStudio 只构建一份正式功能程序，本地打开与客体验收使用相同的 `diskstudio.exe` 和 SHA-256；应用代码没有 VM 身份白名单或专用构建分支。

所有 QEMU、下载、系统盘、测试盘、检查点、工具光盘、测试临时文件和日志集中在项目 `.vm/`。脚本不安装系统服务、不修改 PATH、不提权、不自动启用 Windows 功能。

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

`make-tools-iso.ps1` 使用 IMAPI 文件系统镜像 API 创建普通 ISO 文件，不挂载、不刻录，不访问宿主物理光驱。每次 `-Live` 生成独立 ISO，旧文件仍留在 `.vm/media/`。热切换核对 QMP 身份、三块固定 QCOW2 后端、唯一只读光驱、新文件路径/类型与 SHA-256，不增加共享目录或网络。

WebView2 安装器仅供客体需要时使用；当前客体已有可用运行时，宿主无需安装。

## 功能验收

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
