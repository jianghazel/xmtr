"""Create a test initramfs, including device entries without host mknod access."""

import argparse
import os
from pathlib import Path
import stat

import libarchive


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("root", type=Path)
    parser.add_argument("output", type=Path)
    args = parser.parse_args()
    root = args.root.resolve()
    (root / "init").chmod(0o755)
    (root / "seed").write_bytes(os.urandom(256))
    with libarchive.file_writer(str(args.output), "cpio_newc", "gzip") as archive:
        archive.add_files(str(root), pathname=".", uid=0, gid=0)
        for name, major, minor in [
            ("console", 5, 1),
            ("null", 1, 3),
            ("zero", 1, 5),
            ("random", 1, 8),
            ("urandom", 1, 9),
        ]:
            archive.add_file_from_memory(
                f"dev/{name}",
                0,
                b"",
                filetype=stat.S_IFCHR,
                permission=0o600,
                rdevmajor=major,
                rdevminor=minor,
                uid=0,
                gid=0,
            )


if __name__ == "__main__":
    main()
