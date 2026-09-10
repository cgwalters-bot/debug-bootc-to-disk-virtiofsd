#!/bin/sh
set -eu
bcvk=${1:?path to an isolated bcvk checkout}
test -d "$bcvk/.git"
git -C "$bcvk" apply --check "$(dirname "$0")/bcvk-virtio-serial-fd-forwarding.patch"
git -C "$bcvk" apply "$(dirname "$0")/bcvk-virtio-serial-fd-forwarding.patch"
printf 'patched isolated checkout: %s\n' "$bcvk"
