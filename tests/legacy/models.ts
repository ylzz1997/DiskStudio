import type { Disk as ProductDisk } from "../../src/types";
export { GiB, MiB } from "../../src/types";
export type { Partition } from "../../src/types";
export type Disk = Omit<ProductDisk, "kind"> & {
  kind: ProductDisk["kind"] | "sample";
};
export type Operation = {
  id: string;
  type: "resize" | "format" | "delete" | "create" | "rename";
  partitionId: string;
  name?: string;
  fs?: string;
  size?: number;
};
