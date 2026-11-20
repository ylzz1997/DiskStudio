import { readFile, writeFile } from 'node:fs/promises';
import path from 'node:path';

const source = process.argv[2];
if (!source) throw new Error('Missing extracted NTFS-3G source');

async function edit(name, changes) {
  const filename = path.join(source, name);
  let text = await readFile(filename, 'utf8');
  for (const [before, after] of changes) {
    if (!(before instanceof RegExp ? before.test(text) : text.includes(before))) {
      throw new Error(`Upstream source differs in ${name}`);
    }
    text = text.replace(before, after);
  }
  await writeFile(filename, text);
}

await edit('libntfs-3g/acls.c', [['#include <pwd.h>\n#include <grp.h>\n', '']]);
await edit('libntfs-3g/security.c', [['#include <pwd.h>\n#include <grp.h>\n', '']]);
await edit('libntfs-3g/ioctl.c', [['#include <syslog.h>\n', '']]);
await edit('libntfs-3g/object_id.c', [
  ['inum = ntfs_inode_lookup_by_mbsname(dir_ni,"$ObjId");', `const ntfschar object_name[] = {
			const_cpu_to_le16(0x24), const_cpu_to_le16('O'), const_cpu_to_le16('b'),
			const_cpu_to_le16('j'), const_cpu_to_le16('I'), const_cpu_to_le16('d')
		};
		inum = ntfs_inode_lookup_by_name(dir_ni, object_name, 6);`],
  ['\t\t\t\tntfs_attr_rm(na);\n\t\t\t\tntfs_log_error("Failed to index object id."',
    '\t\t\t\tint error = errno;\n\t\t\t\tntfs_attr_rm(na);\n\t\t\t\terrno = error;\n\t\t\t\tres = -1;\n\t\t\t\tntfs_log_error("Failed to index object id."'],
]);
await edit('libntfs-3g/reparse.c', [
  ['inum = ntfs_inode_lookup_by_mbsname(dir_ni,"$Reparse");', `const ntfschar reparse_name[] = {
			const_cpu_to_le16(0x24), const_cpu_to_le16('R'), const_cpu_to_le16('e'),
			const_cpu_to_le16('p'), const_cpu_to_le16('a'), const_cpu_to_le16('r'),
			const_cpu_to_le16('s'), const_cpu_to_le16('e')
		};
		inum = ntfs_inode_lookup_by_name(dir_ni, reparse_name, 8);`],
]);
await edit('libntfs-3g/efs.c', [
  ['\t\t\ta = ctx->attr;\n\t\t\tna = ntfs_attr_open(ctx->ntfs_ino, AT_DATA,',
    `			a = ctx->attr;
			/* A relocated attribute list can restart enumeration. Do
			   not reopen a finalized stream before the inode flag is set. */
			if (a->flags & ATTR_IS_ENCRYPTED)
				continue;
			/* The size trailer belongs to the start of the stream only. */
			if (a->non_resident && a->lowest_vcn) {
				a->flags |= ATTR_IS_ENCRYPTED;
				NInoSetDirty(ctx->ntfs_ino);
				continue;
			}
			na = ntfs_attr_open(ni, AT_DATA,`],
  ['\tctx->attr->flags |= ATTR_IS_ENCRYPTED;\n\tif (close_ctx)',
    '\tctx->attr->flags |= ATTR_IS_ENCRYPTED;\n\tNInoSetDirty(ctx->ntfs_ino);\n\tif (close_ctx)'],
]);
await edit('libntfs-3g/attrib.c', [
  ['static int ntfs_attr_update_meta(ATTR_RECORD *a',
    'extern const ntfs_attr *diskstudio_sparse_stream;\n\nstatic int ntfs_attr_update_meta(ATTR_RECORD *a'],
  ['\t/* Check whether attribute becomes sparse, unless check is delayed. */',
    '\tif (na == diskstudio_sparse_stream)\n\t\tsparse = 1;\n\n\t/* Check whether attribute becomes sparse, unless check is delayed. */'],
  ['\torg_alloc_size = na->allocated_size;\n\t/* The first cluster outside the new allocation. */\n\tfirst_free_vcn = (newsize + vol->cluster_size - 1) >>\n\t\t\tvol->cluster_size_bits;',
    `	org_alloc_size = na->allocated_size;
	/* The first cluster outside the new allocation. */
	first_free_vcn = (newsize + vol->cluster_size - 1) >>
			vol->cluster_size_bits;
	/* Match the advertised 16-cluster unit without allocating its padding. */
	if (na == diskstudio_sparse_stream) {
		const VCN mask = (1 << STANDARD_COMPRESSION_UNIT) - 1;
		first_free_vcn = (first_free_vcn + mask) & ~mask;
	}`],
]);
// Keep GNU packing active on MinGW for the on-disk structure definitions.
await edit('include/ntfs-3g/compat.h', [
  [/^#define __attribute__\(X\)[^\n]*\n/m, ''],
]);
await edit('libntfs-3g/dir.c', [
  ['index_union __attribute__((__transparent_union__))', 'index_union'],
  ['INDEX_TYPE_ROOT, ir, ie,', 'INDEX_TYPE_ROOT, (index_union){.ir = ir}, ie,'],
  ['INDEX_TYPE_ALLOCATION, ia, ie,', 'INDEX_TYPE_ALLOCATION, (index_union){.ia = ia}, ie,'],
  ['== dir_ni->mft_no)\n\t\t\t\t\t&& !memcmp(fn->file_name, name,',
    '== dir_ni->mft_no)\n\t\t\t\t\t&& fn->file_name_length == len\n\t\t\t\t\t&& !memcmp(fn->file_name, name,'],
  ['/*\n *\t\tSet a DOS name to a file and adjust name spaces', `/* Install the alias next to the unique link in this directory. */
int diskstudio_link_dos_name(ntfs_inode *file, ntfs_inode *dir,
                             const ntfschar *alias, unsigned int units)
{
    if (units > 12 || ntfs_forbidden_names(file->vol, alias, units, TRUE))
        return -1;
    leMFT_REF parent = MK_LE_MREF(dir->mft_no, le16_to_cpu(dir->mrec->sequence_number));
    ntfs_attr_search_ctx *ctx = ntfs_attr_get_search_ctx(file, NULL);
    if (!ctx)
        return -1;
    ntfschar name[NTFS_MAX_NAME_LEN];
    int length = 0;
    int error = 0;
    while (!ntfs_attr_lookup(AT_FILE_NAME, AT_UNNAMED, 0, CASE_SENSITIVE,
                             0, NULL, 0, ctx)) {
        FILE_NAME_ATTR *item = (FILE_NAME_ATTR *)((u8 *)ctx->attr
                              + le16_to_cpu(ctx->attr->value_offset));
        if (item->parent_directory != parent)
            continue;
        if (length || item->file_name_type != FILE_NAME_POSIX || !item->file_name_length) {
            error = EMLINK;
            break;
        }
        length = item->file_name_length;
        memcpy(name, item->file_name, length * sizeof(*name));
    }
    if (!error && errno != ENOENT)
        error = errno;
    ntfs_attr_put_search_ctx(ctx);
    if (error || !length || ntfs_collapsible_chars(file->vol, alias, units, name, length)) {
        errno = error ? error : EINVAL;
        return -1;
    }
    if (set_namespace(file, dir, name, length, FILE_NAME_WIN32) != FILE_NAME_POSIX)
        return -1;
    return ntfs_link_i(file, dir, alias, units, FILE_NAME_DOS);
}

/*
 *\t\tSet a DOS name to a file and adjust name spaces`],
]);
await edit('libntfs-3g/inode.c', [
  ['\t/* Walk through all FILE_NAME attributes and update them. */', `	if (!(ni->flags & FILE_ATTR_REPARSE_POINT)) {
        if (!ntfs_attr_lookup(AT_EA_INFORMATION, AT_UNNAMED, 0,
                              CASE_SENSITIVE, 0, NULL, 0, ctx)) {
            if (ctx->attr->non_resident || le32_to_cpu(ctx->attr->value_length) != sizeof(EA_INFORMATION)) {
                err = EIO;
                goto err_out;
            }
            const EA_INFORMATION *info = (const EA_INFORMATION *)((const u8 *)ctx->attr
                                       + le16_to_cpu(ctx->attr->value_offset));
            reparse_tag = cpu_to_le32(le16_to_cpu(info->ea_length));
        } else if (errno != ENOENT) {
            err = errno;
            goto err_out;
        }
        ntfs_attr_reinit_search_ctx(ctx);
    }
    /* Walk through all FILE_NAME attributes and update them. */`],
  ['\t\t\t/* update or clear the reparse tag in the index */\n\t\tfnx->reparse_point_tag = reparse_tag;',
    '\t\t/* Keep the duplicated reparse tag or packed EA size current. */\n\t\tfn->reparse_point_tag = reparse_tag;\n\t\tfnx->reparse_point_tag = reparse_tag;'],
]);
await edit('ntfsprogs/utils.h', [[/, args\); \} while \(0\)/g, ', ##args); } while (0)']]);
await edit('ntfsprogs/mkntfs.c', [
  [/static BOOL mkntfs_open_partition\(ntfs_volume \*vol\)\n\{[\s\S]*?\n\}/, `static BOOL mkntfs_open_partition(ntfs_volume *vol)
{
    vol->dev = ntfs_device_alloc(opts.dev_name, 0, &ntfs_device_default_io_ops, NULL);
    if (!vol->dev)
        return FALSE;
    return vol->dev->d_ops->open(vol->dev, O_RDWR) == 0;
}`],
  ['int main(int argc, char *argv[])', 'int diskstudio_mkntfs(int argc, char *argv[])'],
  ['ntfs_log_set_handler(ntfs_log_handler_outerr);', 'ntfs_log_set_handler(ntfs_log_handler_stderr);'],
  [/\(\(u64\)random\(\) << 32\) \|\s*\(\(u64\)random\(\) & 0xffffffff\)/,
    'diskstudio_volume_serial'],
  ['typedef enum { WRITE_STANDARD', 'extern uint64_t diskstudio_volume_serial;\n\ntypedef enum { WRITE_STANDARD'],
]);
