import { Channel, invoke } from "@tauri-apps/api/core";
import {
  browseReader,
  formatForName,
  ImageReadServer,
  type ImageReadEvent,
  type NativeImageGroup,
  type SourceRead,
} from "./imageBrowserSource";
import type { ImageBrowseOptions } from "./ImageBrowserPanel";
const servers = new Map<string, ImageReadServer>();
const read: SourceRead = async (group, file, offset, length) =>
  new Uint8Array(
    await invoke<ArrayBuffer>("image_source_read", {
      group,
      file,
      offset,
      length,
    }),
  );
export class ImageSourceConsumedError extends Error {}
export async function openImageBrowser(
  group: NativeImageGroup,
  options: ImageBrowseOptions,
  independent: boolean,
) {
  const format = formatForName(group.files[0]?.name ?? "");
  if (format === "parallels" && !independent)
    throw Error("请确认此 Parallels 文件是独立镜像，不依赖其他快照。");
  const initial = await browseReader(group, options, read);
  const effective = {
    kind: format === "raw" || options.kind === "search" ? options.kind : ("disk" as const),
    sector: initial.sectorSize,
  };
  let first: typeof initial | null = initial;
  const server = new ImageReadServer(
    async () => {
      const image = first ?? (await browseReader(group, effective, read));
      first = null;
      if (
        image.size !== initial.size ||
        image.sectorSize !== initial.sectorSize ||
        image.format !== initial.format
      )
        throw Error("镜像逻辑布局与打开时不一致。");
      return image;
    },
    (frame) => invoke("image_source_reply", frame),
    (session, request, message) =>
      invoke("image_source_error", { session, request, message }),
  );
  const events = new Channel<ImageReadEvent>();
  events.onmessage = (e) => {
    void server.event(e);
    if (e.event === "closed") servers.delete(group.id);
  };
  servers.set(group.id, server);
  try {
    return await invoke<string>("image_source_attach", {
      request: {
        group: group.id,
        size: initial.size,
        format,
        options: effective,
        independent,
        notes: [
          ...initial.warnings,
          ...initial.notes,
          ...(format === "raw"
            ? []
            : [
                "容器中未分配的数据块按零读取；差分链只使用明确选定且已核对的父盘，不代表历史块仍可恢复。",
              ]),
        ],
      },
      events,
    });
  } catch (e) {
    servers.delete(group.id);
    await server.event({ event: "closed", session: "" });
    throw new ImageSourceConsumedError(String(e));
  }
}
