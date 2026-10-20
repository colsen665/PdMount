#!/usr/bin/env bash
# M1 content check (DESIGN.md §16): every file of the dataset, including
# the 5 MiB, 41 MiB and 2 GiB files, has the sha256 it was generated with.
source "$(dirname "$0")/lib.sh"
ensure_dataset
prepare_mount

step "sha256 [$VERIFY_MODE]"
(cd "$REMOTE" && time sha256sum --quiet -c "$LOCAL/MANIFEST.sha256") || fail "checksum mismatch"
echo "all $(wc -l <"$LOCAL/MANIFEST.sha256") files match"
for file in big/5MiB.bin big/41MiB.bin big/2GiB.bin; do
    local_size=$(stat -c %s "$LOCAL/$file")
    remote_size=$(stat -c %s "$REMOTE/$file")
    [[ "$local_size" == "$remote_size" ]] || fail "$file: size $remote_size, expected $local_size"
done
echo "sizes from the API match"
check_hot_path
echo "sha256 [$VERIFY_MODE]: PASS"
