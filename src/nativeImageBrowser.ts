import { Channel, invoke } from "@tauri-apps/api/core";
import {
  browseReader,
  formatForName,
  retainedImageFile,
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
export const retainedPrimaryImage = (group: NativeImageGroup) =>
  retainedImageFile(group.id, group.files[0], read);
export async function openImageBrowser(
  group: NativeImageGroup,
  options: ImageBrowseOptions,
  independent: boolean,
  parallelsSnapshot?: string,
) {
  const format = formatForName(group.files[0]?.name ?? "");
  if (format === "parallels" && !independent)
    throw Error("请确认此 Parallels 文件是独立镜像，不依赖其他快照。");
  const initial = await browseReader(group, options, read, parallelsSnapshot);
  const snapshot = initial.parallelsSnapshot?.id;
  const effective = {
    kind: format === "raw" || options.kind === "search" ? options.kind : ("disk" as const),
    sector: initial.sectorSize,
  };
  let first: typeof initial | null = initial;
  const server = new ImageReadServer(
    async () => {
      const image = first ?? (await browseReader(group, effective, read, snapshot));
      first = null;
      if (
        image.size !== initial.size ||
        image.sectorSize !== initial.sectorSize ||
        image.format !== initial.format || image.parallelsSnapshot?.id !== snapshot
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
        parallelsSnapshot: snapshot,
        notes: [
          ...initial.warnings,
          ...initial.notes,
          ...(format === "raw"
            ? []
            : [
                "镜像按容器的块映射读取。差分链会从已核对的父盘读取继承的数据。",
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
