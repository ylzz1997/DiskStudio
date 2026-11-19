"""Verify and unpack ordinary Btrfs images from the Ubuntu guest."""
import base64
import gzip
import hashlib
import io
import json
from pathlib import Path
import re
import sys


def extract(text, case, output, name, fields):
    marker = f'DS_BTRFS_{case}'
    if f'DISKSTUDIO_BTRFS_{case}_FIXTURE_PASS' not in text:
        raise ValueError(f'Missing Linux verification for {case}')
    manifests = re.findall(rf'^{marker}_MANIFEST (\{{.+\}})$', text, re.MULTILINE)
    if len(manifests) != 1:
        raise ValueError('Expected exactly one Btrfs fixture manifest')
    evidence = json.loads(manifests[0])
    if case == 'SNAPSHOT':
        evidence.setdefault('kind', 'snapshot')
    if any(evidence.get(key) != value for key, value in fields.items()):
        raise ValueError('Wrong class of Btrfs fixture')
    if case == 'RETAINED' and not evidence.get('oldTree'):
        raise ValueError('Missing retained Btrfs root')
    blocks = re.findall(rf'^{marker}_BLOCK (\d{{5}}) ([A-Za-z0-9+/=]+)$', text, re.MULTILINE)
    if not blocks or any(int(index) != n or len(encoded) > 768 for n, (index, encoded) in enumerate(blocks)):
        raise ValueError('Missing, extra, or misordered fixture bytes')
    archive = b''.join(base64.b64decode(encoded, validate=True) for _, encoded in blocks)
    if len(archive) != evidence['gzipBytes'] or hashlib.sha256(archive).hexdigest() != evidence['gzipSha256']:
        raise ValueError('Compressed fixture differs from the guest manifest')
    size = 256 * 1024 * 1024
    with gzip.GzipFile(fileobj=io.BytesIO(archive)) as stream:
        image = stream.read(size + 1)
    if len(image) != size or hashlib.sha256(image).hexdigest() != evidence['imageSha256']:
        raise ValueError('Decoded fixture differs from the guest manifest')
    output.mkdir(exist_ok=True)
    if output.resolve() != output:
        raise ValueError('Fixture output directory must be in .vm')
    with (output / f'{name}.raw').open('xb') as stream:
        stream.write(image)
    with (output / f'{name}-manifest.json').open('x', encoding='utf-8') as stream:
        json.dump(evidence, stream, indent=2)
        stream.write('\n')
    print(evidence['imageSha256'])


def main():
    vm = Path(__file__).resolve().parents[2] / '.vm'
    log = Path(sys.argv[1]).resolve(strict=True)
    match = re.fullmatch(r'btrfs-(snapshot-image|retained-image|checksum-images|members-images|parity-images|allocation-image)-\d+\.log', log.name)
    if log.parent != vm / 'ubuntu' or not match:
        raise ValueError('Expected the verified Ubuntu Btrfs serial log under .vm')
    text = log.read_text(encoding='utf-8', errors='replace')
    if match[1] == 'members-images':
        if 'no error found' not in text:
            raise ValueError('Missing Linux multi-device data verification')
        for index in (1, 2):
            extract(text, f'MEMBER_{index}', vm / 'btrfs-native-members',
                    f'member-{index}', {'member': index})
    elif match[1] == 'parity-images':
        if text.count('no error found') != 2:
            raise ValueError('Expected Linux data verification for RAID5 and RAID6')
        for profile, count in [('raid5', 2), ('raid6', 4)]:
            for index in range(1, count + 1):
                extract(text, f'{profile.upper()}_{index}', vm / 'btrfs-native-parity',
                        f'{profile}-{index}', {'profile': profile, 'member': index})
    elif match[1] == 'checksum-images':
        algorithms = ('xxhash', 'sha256', 'blake2')
        if text.count('no error found') != len(algorithms):
            raise ValueError('Expected Linux data verification for each checksum')
        for algorithm in algorithms:
            extract(text, f'CHECKSUM_{algorithm.upper()}', vm / 'btrfs-native-checksum',
                    algorithm, {'algorithm': algorithm})
    else:
        kind = match[1].removesuffix('-image')
        if 'no error found' not in text:
            raise ValueError('Missing Linux history fixture verification')
        extract(text, kind.upper(), vm / f'btrfs-native-{kind}', kind, {'kind': kind})


if __name__ == '__main__':
    main()
