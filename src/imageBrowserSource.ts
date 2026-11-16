import { openContainer } from "./imageReader";
import { logicalLimit, type ImageFile, type LogicalImage } from "./imageSource";
import type { ImageBrowseOptions } from "./ImageBrowserPanel";

export type NativeImageFile = {
  id: string;
  name: string;
  size: number;
  lastModified: number;
};
export type NativeImageGroup = { id: string; files: NativeImageFile[] };
export type SourceRead = (
  group: string,
  file: string,
  at: number,
  length: number,
) => Promise<Uint8Array>;
export type ContainerFormat =
  "raw" | "vhd" | "vhdx" | "vdi" | "vmdk" | "parallels";
export const MiB = 1024 ** 2;
export function formatForName(name: string): ContainerFormat {
  if (/\.(img|raw)$/i.test(name)) return "raw";
  if (/\.a?vhd$/i.test(name)) return "vhd";
  if (/\.vhdx$/i.test(name)) return "vhdx";
  if (/\.vdi$/i.test(name)) return "vdi";
  if (/\.vmdk$/i.test(name)) return "vmdk";
  if (/\.(hdd|hds)$/i.test(name)) return "parallels";
  throw Error("不支持此镜像文件类型。");
}
export function retainedImageFile(
  group: string,
  info: NativeImageFile,
  read: SourceRead,
): ImageFile {
  if (
    !group ||
    !info.id ||
    !info.name ||
    info.name.length > 255 ||
    /[\\/:\0]/.test(info.name) ||
    !Number.isSafeInteger(info.size) ||
    info.size < 1 ||
    info.size > 2 ** 44 ||
    !Number.isSafeInteger(info.lastModified) ||
    info.lastModified < 0
  )
    throw Error("保留的镜像文件元数据无效。");
  // Copy fields: a later draft/companion selection cannot mutate this reader.
  const { id, name, size, lastModified } = info;
  const normalize = (n: number) => {
    if (!Number.isFinite(n)) throw Error("镜像切片范围无效。");
    n = Math.trunc(n);
    return n < 0 ? Math.max(0, size + n) : Math.min(n, size);
  };
  return Object.freeze({
    name,
    size,
    lastModified,
    slice(start = 0, end = size) {
      const at = normalize(start),
        stop = Math.max(at, normalize(end));
      if (stop - at > 64 * MiB) throw Error("单次镜像元数据切片超过 64 MiB。");
      return {
        async arrayBuffer() {
          const bytes = new Uint8Array(stop - at);
          for (let done = 0; done < bytes.length;) {
            const length = Math.min(MiB, bytes.length - done);
            const chunk = await read(group, id, at + done, length);
            if (chunk.length !== length)
              throw Error("镜像源文件读取不完整；不会补零。");
            bytes.set(chunk, done);
            done += length;
          }
          return bytes.buffer;
        },
      };
    },
  });
}
export async function browseReader(
  group: NativeImageGroup,
  options: ImageBrowseOptions,
  read: SourceRead,
): Promise<LogicalImage> {
  if (group.files.length < 1 || group.files.length > 64)
    throw Error("镜像文件数量无效。");
  const names = group.files.map((f) => f.name.toLocaleLowerCase("en-US"));
  if (new Set(names).size !== names.length)
    throw Error("镜像来源包含同名文件。");
  const [file, ...companions] = group.files.map((f) =>
    retainedImageFile(group.id, f, read),
  );
  const format = formatForName(file.name);
  if (companions.length && !["vhd", "vmdk"].includes(format))
    throw Error("此镜像格式不接受父盘或分卷。");
  if (format !== "raw") {
    const image = await openContainer(file, companions, "browse");
    if (!image) throw Error("镜像容器读取器不可用。");
    await image.verify();
    return image;
  }
  if (
    companions.length ||
    ![512, 4096].includes(options.sector) ||
    file.size < MiB ||
    file.size > 2 ** 43 ||
    file.size % options.sector
  )
    throw Error("RAW/IMG 须为 1 MiB–8 TiB，按所选扇区对齐，不接受父盘或分卷。");
  let total = 0;
  return {
    size: file.size,
    format: "RAW",
    sectorSize: options.sector,
    notes: [],
    warnings: [],
    async verify() {},
    async read(at, length) {
      if (
        !Number.isSafeInteger(at) ||
        !Number.isSafeInteger(length) ||
        at < 0 ||
        length < 1 ||
        length > MiB ||
        at > file.size - length ||
        (total += length) > logicalLimit("browse", file.size)
      )
        throw Error("RAW/IMG 读取越界或超过本次操作预算。");
      return new Uint8Array(await file.slice(at, at + length).arrayBuffer());
    },
  };
}
function uuidBytes(id: string) {
  if (
    !/^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$/i.test(id)
  )
    throw Error("镜像数据通道标识无效。");
  return new Uint8Array(
    id
      .replaceAll("-", "")
      .match(/../g)!
      .map((s) => Number.parseInt(s, 16)),
  );
}
export function imageReplyFrame(
  session: string,
  request: string,
  bytes: Uint8Array,
) {
  if (!bytes.length || bytes.length > 65536)
    throw Error("镜像解码响应长度无效。");
  const frame = new Uint8Array(32 + bytes.length);
  frame.set(uuidBytes(session));
  frame.set(uuidBytes(request), 16);
  frame.set(bytes, 32);
  return frame;
}
export type ImageReadEvent =
  | {
      event: "read";
      session: string;
      reader: string;
      request: string;
      offset: number;
      length: number;
    }
  | { event: "readerClosed"; session: string; reader: string }
  | { event: "closed"; session: string };

// Lifetime follows native source sessions, not React pages. Every operation gets
// a fresh decoder; closing a reader prevents its late async response publishing.
export class ImageReadServer {
  private readers = new Map<
    string,
    { closed: boolean; image: Promise<LogicalImage> }
  >();
  private closed = false;
  constructor(
    private create: () => Promise<LogicalImage>,
    private reply: (frame: Uint8Array) => Promise<unknown>,
    private fail: (
      session: string,
      request: string,
      message: string,
    ) => Promise<unknown>,
  ) {}
  async event(event: ImageReadEvent): Promise<void> {
    if (event.event === "closed") {
      this.closed = true;
      for (const c of this.readers.values()) c.closed = true;
      this.readers.clear();
      return;
    }
    if (event.event === "readerClosed") {
      const c = this.readers.get(event.reader);
      if (c) c.closed = true;
      this.readers.delete(event.reader);
      return;
    }
    if (this.closed) return;
    let context = this.readers.get(event.reader);
    try {
      if (
        !Number.isSafeInteger(event.offset) ||
        event.offset < 0 ||
        !Number.isSafeInteger(event.length) ||
        event.length < 1 ||
        event.length > 65536
      )
        throw Error("镜像逻辑读取请求无效。");
      if (!context) {
        if (this.readers.size >= 4) throw Error("镜像独立读取器数量超过上限。");
        context = { closed: false, image: this.create() };
        this.readers.set(event.reader, context);
      }
      const image = await context.image;
      if (this.closed || context.closed) return;
      if (event.offset > image.size - event.length)
        throw Error("镜像逻辑读取越界。");
      const bytes = await image.read(event.offset, event.length);
      if (this.closed || context.closed) return;
      if (bytes.length !== event.length) throw Error("镜像解码返回长度不符。");
      // A rejected/stale response is terminal; never retry a response or source read.
      await this.reply(
        imageReplyFrame(event.session, event.request, bytes),
      ).catch(() => undefined);
    } catch (e) {
      if (!this.closed && !context?.closed)
        await this.fail(
          event.session,
          event.request,
          String(e).slice(0, 1000),
        ).catch(() => undefined);
    }
  }
}
