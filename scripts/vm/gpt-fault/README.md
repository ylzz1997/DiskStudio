# B 盘 GPT 故障夹具

此程序只用于 `DiskStudio-Test` 的单份 GPT 修复验收，不属于正式应用。宿主只编译和运行纯模型单元测试，**不得在宿主启动 release 程序**。

先运行 `node scripts/vm/lab.mjs preflight` 核对 VM 名称、UUID、隔离文件盘和容量。A/B 必须已脱机；在客体核对 B 完整序列号 `DS-TEST-B-0001      _00000001.`、16 GiB、512 字节扇区、9 个分区。将编译产物置于 `.vm/payload/GPT-Fault-B.exe`，通过不可变只读工具光盘在客体启动。

程序拒绝参数，先核对固定 VM UUID、三盘完整清单、两个 WMI 提供者的编号/完整序列号、MSFT_Disk 精确容量和保护状态，再显示目标确认。确认后重复核对，只打开固定的 B 盘设备句柄，并验证原生编号、精确容量、扇区、完整序列号与脱机属性。只有两份 GPT 都完整且一致时，才翻转末扇区 GPT 头部 CRC 字段的一个位；一次写入 512 字节，刷新后比对首尾各 1 MiB，确认只有预期字节改变。它不使磁盘联机、不修复、不重试、不回滚。

使用 `scripts/vm/build-gpt-fault.ps1` 静态链接运行库并暂存到只读光盘目录。旧 [Win32_DiskDrive.Size](https://learn.microsoft.com/windows/win32/cimwin32prov/win32-diskdrive) 是柱面几何乘积，不能用作精确字节容量；容量必须同时通过 MSFT_Disk 和原生 `IOCTL_DISK_GET_LENGTH_INFO` 核验。

使用正式 DiskStudio 的“磁盘 → 分区表检查与修复”完成后续检查、预览、确认和回读。修复后检查快照的 SHA-256 应等于夹具给出的基线值。夹具的失败不代表可以放宽身份保护；先检查具体原因。

构建产物和日志保存在 `.vm/gpt-fault-build` 与 `.vm` 下；本目录只保存源代码和依赖锁文件。
