// Only explicitly selected bytes. Native files expose the same bounded slice
// interface through opaque retained-handle capabilities, never frontend paths.
export interface ImageFile {
  name: string;
  size: number;
  lastModified: number;
  slice(start?: number, end?: number): { arrayBuffer(): Promise<ArrayBuffer> };
}
export type ImageReadMode = boolean | "browse";
export const logicalLimit = (mode: ImageReadMode, size: number) =>
  mode === "browse" ? size * 16 + 64 * 1024 ** 2 : mode ? size : 4 * 1024 ** 2;
export const fileFactor = (mode: ImageReadMode) => (mode === "browse" ? 64 : 4);
// A logical read-only view. Each browsing operation opens a fresh parser and
// gets its own bounded budget; partition inventory retains the smaller budget.
export interface LogicalImage {
  size: number;
  format: string;
  fileSize?: number;
  fileCount?: number;
  chain?: string[];
  sectorSize: 512 | 4096;
  warnings: string[];
  notes: string[];
  read(offset: number, length: number): Promise<Uint8Array>;
  verify(): Promise<void>;
}
