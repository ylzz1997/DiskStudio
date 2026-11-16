`ntfs-testfs1.img` is the unmodified 2 MiB `testdata/testfs1` fixture from the
`ntfs` crate 0.4.0, copied from the local Cargo registry. SHA-256:
`e3612c182b8010e3599b5eb93bff427c7d824e85bdc2ddbe46e378e3ba814eb9`.

Upstream: https://github.com/ColinFinck/ntfs/tree/v0.4.0/testdata
Copyright 2021 Colin Finck. Used under MIT; see `ntfs-testfs1-LICENSE.txt`.

Tests include these bytes under `cfg(test)` and modify only an in-memory Vec to
exercise nested, reused and cyclic NTFS directory references. This file is not
mounted, opened as a device, or bundled in the production application. The
upstream fixture creation script is not executed.
