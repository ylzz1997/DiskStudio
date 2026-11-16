export class MissingImageFilesError extends Error {
  constructor(
    public missing: string[],
    public kind: "data" | "parent" = "data",
  ) {
    super(
      kind === "parent"
        ? "请选择差分镜像所需的父盘文件。"
        : "请选择镜像配套的数据文件。",
    );
  }
}
