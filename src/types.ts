export type BitLockerState =
  | "encrypted"
  | "encrypting"
  | "decrypting"
  | "encryptionPaused"
  | "decryptionPaused"
  | "unknown";

export interface Partition {
  id: string;
  name: string;
  fs: string;
  start: number;
  size: number;
  used: number | null;
  color: string;
  system: boolean;
  number?: number;
  driveLetter?: string | null;
  role?: string;
  // Verified OS partition only; never inferred from filesystem or partition type.
  platform?: "windows" | "linux" | null;
  volumeSize?: number | null;
  volumeLabel?: string | null;
  available?: number | null;
  volumeStatus?: string;
  encryption?: BitLockerState;
  hidden?: boolean;
}
export interface HardwareInfo {
  identity?: string | null;
  layoutKey?: string | null;
  number: number;
  model: string;
  bus: string;
  health: string;
  isSystem: boolean;
  offline: boolean | null;
  deviceReadOnly: boolean | null;
  layoutAvailable: boolean;
  layoutNote: string | null;
}
export type TableCheckState = "passed" | "warning" | "failed" | "unchecked";
export interface TableReport {
  filename: string;
  scheme: string;
  sectorSize: number | null;
  state: TableCheckState;
  items: {
    code: string;
    title: string;
    state: TableCheckState;
    detail: string;
  }[];
}
export interface Disk {
  id: string;
  name: string;
  filename: string;
  size: number;
  sectorSize: number;
  scheme: string;
  kind: "image" | "physical";
  partitions: Partition[];
  hardware?: HardwareInfo;
  tableReport?: TableReport;
  imageFormat?: string;
  imageFileSize?: number;
  imageFileCount?: number;
  imageChain?: string[];
  imageNotes?: string[];
}
export interface DiskSnapshot {
  disks: Disk[];
  observedAt: number;
  warnings: string[];
}
export interface DataPlan {
  token: string;
  title: string;
  source: string;
  destination: string;
  bytes: number;
  warning: string;
  destructive: boolean;
}
export interface PartitionTarget {
  target: string;
  diskId: string;
  name: string;
  serial: string;
  revision: string;
  freeRanges: { offset: number; size: number }[];
  formatPartitions: number[];
  deletePartitions: number[];
  resizePartitions: number[];
  fatResizePartitions: number[];
  extPartitions: number[];
  metadataPartitions: number[];
  availableDriveLetters: string[];
  layoutKey: string;
  available: boolean;
  reason: string | null;
}
export interface PartitionStatus {
  enabled: boolean;
  reason: string | null;
  targets: PartitionTarget[];
}
export type WindowsFileSystem = "NTFS" | "FAT12" | "FAT16" | "FAT32" | "exFAT";
export interface FormatOptions {
  mode: "quick" | "full";
  ntfsCompression: boolean;
  ntfsShortNames: boolean;
}
export type ExtFileSystem = "ext2" | "ext3" | "ext4";
export type PartitionAction =
  | { type: "initialize" }
  | { type: "create"; offset: number; size: number; kind: "windows" | "linux" }
  | {
      type: "format";
      partition: number;
      label: string;
      fileSystem: WindowsFileSystem;
      clusterSize: number;
      options: FormatOptions;
    }
  | { type: "resize"; partition: number; size: number }
  | { type: "fatResize"; partition: number; size: number }
  | {
      type: "extFormat";
      partition: number;
      label: string;
      fileSystem: ExtFileSystem;
      blockSize: number;
    }
  | { type: "extResize"; partition: number; size: number }
  | { type: "btrfsFormat"; partition: number; label: string }
  | { type: "btrfsResize"; partition: number; size: number }
  | { type: "setLabel"; partition: number; label: string }
  | { type: "setDriveLetter"; partition: number; letter: string | null }
  | { type: "delete"; partition: number };
export interface PartitionPlan {
  token: string;
  title: string;
  targetName: string;
  serial: string;
  partition: number | null;
  beforeSize: number | null;
  afterSize: number | null;
  warning: string;
  expiresAt: number;
}
export const MiB = 1024 ** 2;
export const GiB = 1024 ** 3;
export const TiB = 1024 ** 4;
export const formatSize = (bytes: number | null, digits = 2) => {
  if (bytes === null) return "—";
  const unit =
    bytes >= TiB
      ? TiB
      : bytes >= GiB
        ? GiB
        : bytes >= MiB
          ? MiB
          : bytes >= 1024
            ? 1024
            : 1;
  return `${Number((bytes / unit).toFixed(digits))} ${unit === TiB ? "TiB" : unit === GiB ? "GiB" : unit === MiB ? "MiB" : unit === 1024 ? "KiB" : "B"}`;
};
