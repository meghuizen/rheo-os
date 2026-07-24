#!/bin/sh
# Regenerate the read-only ext4 test image (tests/fixtures/ext4.img) with a
# known layout the `posix` test kernel asserts against. Needs e2fsprogs
# (mkfs.ext4, debugfs). The image is committed so the test build has no such
# dependency; run this only to change the fixture.
set -e
dir=$(dirname "$0")
img="$dir/ext4.img"
tmp=$(mktemp -d)
dd if=/dev/zero of="$img" bs=1024 count=512 2>/dev/null
# 1 KiB blocks; disable journal/csums/htree/64bit/resize for a simple,
# stable-to-parse layout. Extents (ext4 default) stay on.
mkfs.ext4 -q -b 1024 \
  -O ^has_journal,^metadata_csum,^64bit,^resize_inode,^dir_index,extent \
  -F "$img"
printf 'hello from ext4\n' > "$tmp/hello.txt"
printf 'The quick brown fox jumps over the lazy dog.\n' > "$tmp/fox.txt"
# A multi-block file (>1 block) to exercise extent-mapped reads.
awk 'BEGIN{for(i=0;i<200;i++)printf "line %03d: the lattice filesystem works\n", i}' > "$tmp/big.txt"
debugfs -w -R "mkdir /docs" "$img" >/dev/null 2>&1
debugfs -w -R "write $tmp/hello.txt hello.txt" "$img" >/dev/null 2>&1
debugfs -w -R "write $tmp/fox.txt docs/fox.txt" "$img" >/dev/null 2>&1
debugfs -w -R "write $tmp/big.txt docs/big.txt" "$img" >/dev/null 2>&1
echo "wrote $img ($(wc -c < "$img") bytes)"
echo "big.txt size: $(wc -c < "$tmp/big.txt") bytes"
