// MS-VHDX v1 section 2.3. Apply a committed log to an in-memory view.
const SECTOR = 4096;
const MiB = 1024 ** 2;
const ENTRY_HEADER = 64;
const DESCRIPTOR = 32;
export const VHDX_LOG_LIMIT = 16 * MiB;

const crcTable = Uint32Array.from({ length: 256 }, (_, n) => {
  for (let bit = 0; bit < 8; bit++) n = (n >>> 1) ^ (n & 1 ? 0x82f63b78 : 0);
  return n >>> 0;
});
export function crc32c(bytes: Uint8Array, zeroAt = -1) {
  let crc = 0xffffffff;
  for (let i = 0; i < bytes.length; i++) {
    const value = zeroAt >= 0 && i >= zeroAt && i < zeroAt + 4 ? 0 : bytes[i];
    crc = (crc >>> 8) ^ crcTable[(crc ^ value) & 255];
  }
  return ~crc >>> 0;
}

type Span = { at: number; end: number };
type Write = Span & { bytes?: Uint8Array };
type Entry = {
  at: number;
  length: number;
  tail: number;
  sequence: bigint;
};

function view(bytes: Uint8Array) {
  return new DataView(bytes.buffer, bytes.byteOffset, bytes.byteLength);
}

function entryBytes(log: Uint8Array, at: number, length: number) {
  if (at + length <= log.length) return log.subarray(at, at + length);
  const result = new Uint8Array(length);
  result.set(log.subarray(at));
  result.set(log.subarray(0, at + length - log.length), log.length - at);
  return result;
}

function descriptorEnd(count: number) {
  return Math.ceil((ENTRY_HEADER + count * DESCRIPTOR) / SECTOR) * SECTOR;
}

function validDescriptors(bytes: Uint8Array, count: number, sequence: bigint) {
  const data = view(bytes);
  let sector = descriptorEnd(count);
  for (let i = 0; i < count; i++) {
    const at = ENTRY_HEADER + i * DESCRIPTOR;
    const signature = data.getUint32(at, true);
    if (
      data.getBigUint64(at + 24, true) !== sequence ||
      data.getUint32(at + 16, true) % SECTOR
    )
      return false;
    if (signature === 0x6f72657a) {
      // zero
      if (
        data.getUint32(at + 4, true) !== 0 ||
        data.getUint32(at + 8, true) % SECTOR
      )
        return false;
    } else if (signature === 0x63736564) {
      // desc
      if (
        sector >= bytes.length ||
        data.getUint32(sector, true) !== 0x61746164 || // data
        data.getUint32(sector + 4, true) !== data.getUint32(20, true) ||
        data.getUint32(sector + SECTOR - 4, true) !== data.getUint32(16, true)
      )
        return false;
      sector += SECTOR;
    } else return false;
  }
  return sector === bytes.length;
}

function currentSequence(log: Uint8Array, guid: Uint8Array) {
  const data = view(log);
  const entries = new Map<number, Entry>();
  let checkedBytes = 0;
  for (let at = 0; at < log.length; at += SECTOR) {
    if (
      data.getUint32(at, true) !== 0x65676f6c || // loge
      !guid.every((byte, i) => byte === log[at + 32 + i])
    )
      continue;
    const length = data.getUint32(at + 8, true);
    const tail = data.getUint32(at + 12, true);
    const sequence = data.getBigUint64(at + 16, true);
    const count = data.getUint32(at + 24, true);
    if (
      length > log.length ||
      length % SECTOR ||
      length < descriptorEnd(count) ||
      tail >= log.length ||
      tail % SECTOR ||
      sequence === 0n ||
      data.getUint32(at + 28, true) !== 0 ||
      data.getUint32(at + 48, true) % MiB ||
      data.getUint32(at + 56, true) % MiB
    )
      continue;
    // Valid ring entries occupy disjoint sectors. Bound hashing work if a
    // damaged ring contains many overlapping candidate headers.
    checkedBytes += length;
    if (checkedBytes > 4 * log.length)
      throw Error("VHDX 日志候选条目超过校验资源上限。");
    const bytes = entryBytes(log, at, length);
    if (
      crc32c(bytes, 4) === data.getUint32(at + 4, true) &&
      validDescriptors(bytes, count, sequence)
    )
      entries.set(at, { at, length, tail, sequence });
  }

  let active: Entry[] = [];
  const heads = [...entries.values()].sort((a, b) =>
    a.sequence === b.sequence ? 0 : a.sequence > b.sequence ? -1 : 1,
  );
  for (const head of heads) {
    if (active.length && active.at(-1)!.sequence > head.sequence) break;
    const sequence: Entry[] = [];
    let used = 0;
    for (let entry = entries.get(head.tail); entry;) {
      used += entry.length;
      if (used > log.length) break;
      sequence.push(entry);
      if (entry === head) {
        if (active.length)
          throw Error("VHDX 日志存在同序号的完整序列，无法确认当前状态。");
        active = sequence;
        break;
      }
      const next = entries.get((entry.at + entry.length) % log.length);
      if (!next || next.sequence !== entry.sequence + 1n) break;
      entry = next;
    }
  }
  if (!active.length) throw Error("VHDX 日志没有可校验的完整序列。");
  return active;
}

function finalWrites(writes: Write[]) {
  // Split only at descriptor endpoints, including when a zero descriptor
  // spans a large unallocated range. Visit each completed interval once.
  const endpoints = [
    ...new Set(writes.flatMap(({ at, end }) => [at, end])),
  ].sort((a, b) => a - b);
  const index = new Map(endpoints.map((at, i) => [at, i]));
  const next = Uint32Array.from(endpoints, (_, i) => i);
  const covered: (Write | undefined)[] = new Array(endpoints.length);
  function available(start: number) {
    while (next[start] !== start) {
      next[start] = next[next[start]];
      start = next[start];
    }
    return start;
  }
  for (let i = writes.length - 1; i >= 0; i--) {
    const write = writes[i];
    const stop = index.get(write.end)!;
    for (
      let part = available(index.get(write.at)!);
      part < stop;
      part = next[part]
    ) {
      covered[part] = {
        at: endpoints[part],
        end: endpoints[part + 1],
        bytes: write.bytes,
      };
      next[part] = available(part + 1);
    }
  }
  return covered.filter((write): write is Write => write !== undefined);
}

export function replayVhdxLog(
  log: Uint8Array,
  guid: Uint8Array,
  logAt: number,
  physicalSize: number,
  fileLimit: number,
) {
  const sequence = currentSequence(log, guid);
  const ring = view(log);
  const head = sequence.at(-1)!.at;
  if (ring.getBigUint64(head + 48, true) > BigInt(physicalSize))
    throw Error("VHDX 文件短于日志记录的已持久化长度。");
  const last = ring.getBigUint64(head + 56, true);
  if (last > BigInt(fileLimit))
    throw Error("VHDX 日志回放后的文件长度超出支持范围。");
  let size = Math.max(physicalSize, Number(last));
  const writes: Write[] = [];
  for (const entry of sequence) {
    const bytes = entryBytes(log, entry.at, entry.length);
    const data = view(bytes);
    const count = data.getUint32(24, true);
    let sector = descriptorEnd(count);
    for (let i = 0; i < count; i++) {
      const descriptor = ENTRY_HEADER + i * DESCRIPTOR;
      const zero = data.getUint32(descriptor, true) === 0x6f72657a;
      const start = data.getBigUint64(descriptor + 16, true);
      const length = zero
        ? data.getBigUint64(descriptor + 8, true)
        : BigInt(SECTOR);
      const stop = start + length;
      if (stop > BigInt(fileLimit))
        throw Error("VHDX 日志更新超出支持的文件范围。");
      if (length === 0n) continue;
      const at = Number(start),
        end = Number(stop);
      if (
        (at < MiB && (at < 192 * 1024 || end > 320 * 1024)) ||
        (at < logAt + log.length && end > logAt)
      )
        throw Error("VHDX 日志试图覆盖容器头部或日志区域。");
      size = Math.max(size, end);
      const write: Write = { at, end };
      if (!zero) {
        write.bytes = bytes.slice(sector, sector + SECTOR);
        write.bytes.set(bytes.subarray(descriptor + 8, descriptor + 16));
        write.bytes.set(
          bytes.subarray(descriptor + 4, descriptor + 8),
          SECTOR - 4,
        );
        sector += SECTOR;
      }
      writes.push(write);
    }
  }
  const patches = finalWrites(writes);
  function firstEndingAfter(at: number) {
    let lo = 0,
      hi = patches.length;
    while (lo < hi) {
      const mid = (lo + hi) >>> 1;
      if (patches[mid].end <= at) lo = mid + 1;
      else hi = mid;
    }
    return lo;
  }
  return {
    size,
    intersects(at: number, end: number) {
      const first = patches[firstEndingAfter(at)];
      return first !== undefined && first.at < end;
    },
    apply(bytes: Uint8Array<ArrayBuffer>, at: number) {
      const end = at + bytes.length;
      let result = bytes;
      for (
        let i = firstEndingAfter(at);
        i < patches.length && patches[i].at < end;
        i++
      ) {
        const patch = patches[i];
        const from = Math.max(at, patch.at),
          to = Math.min(end, patch.end);
        if (result === bytes) result = bytes.slice();
        if (patch.bytes)
          result.set(
            patch.bytes.subarray(from - patch.at, to - patch.at),
            from - at,
          );
        else result.fill(0, from - at, to - at);
      }
      return result;
    },
  };
}
