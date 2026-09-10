ARG BASE_IMAGE=quay.io/centos-bootc/centos-bootc:stream9
FROM rust:1.82 AS build
RUN rustup target add x86_64-unknown-linux-musl
WORKDIR /src
COPY guest-collector guest-collector
RUN cargo build --release --manifest-path guest-collector/Cargo.toml --target x86_64-unknown-linux-musl

FROM ${BASE_IMAGE} AS rootfs
ARG TARGETARCH
COPY --from=build /src/guest-collector/target/x86_64-unknown-linux-musl/release/bootc-debug-collector /usr/libexec/bootc-debug-collector
COPY --from=build /src/guest-collector/target/x86_64-unknown-linux-musl/release/guest-irq-test /usr/libexec/guest-irq-test
COPY units/bootc-debug-collector.service /usr/lib/systemd/system/bootc-debug-collector.service
COPY units/bootc-debug-irq-test.service /usr/lib/systemd/system/bootc-debug-irq-test.service
COPY units/bootc-debug-fwcfg.conf /etc/modules-load.d/bootc-debug-fwcfg.conf
RUN chmod 0755 /usr/libexec/bootc-debug-collector /usr/libexec/guest-irq-test && \
    systemctl enable bootc-debug-collector.service
# The base workload image may contain an older live probe reader. Disable only
# its unit; do not clear tracefs state owned by another service at runtime.
RUN systemctl disable live-probe.service probe.service 2>/dev/null || true
RUN dnf -y install strace procps-ng lsof util-linux trace-cmd perf gdb kmod || \
    (echo 'WARNING: one or more optional debug packages are unavailable; collector remains usable' >&2; true)
RUN rm -f /boot/EFI/Linux/*.efi

# The sealed base does not retain raw kernel/initramfs files. Extract the exact
# already-tested 742 kernel from its UKI so the regenerated UKI uses identical
# boot inputs while embedding the digest of the debug rootfs.
FROM ${BASE_IMAGE} AS kernel
RUN kver=$(basename /boot/EFI/Linux/*.efi .efi) && \
    mkdir -p "/out/${kver}" && \
    uki=$(ls /boot/EFI/Linux/*.efi) && \
    objcopy --dump-section .linux="/out/${kver}/vmlinuz" "$uki" && \
    objcopy --dump-section .initrd="/out/${kver}/initramfs.img" "$uki"

FROM ${BASE_IMAGE} AS sealed-uki
RUN mkdir -p /out
# seal-uki expects the standard BuildKit secret paths and computes the digest
# from the complete post-debug rootfs; no missing-verity option is permitted.
RUN --mount=type=bind,from=rootfs,source=/,target=/run/target \
    --mount=type=bind,from=kernel,source=/out,target=/run/kernel \
    --mount=type=secret,id=secureboot_key,target=/run/secrets/secureboot_key \
    --mount=type=secret,id=secureboot_cert,target=/run/secrets/secureboot_cert \
    kver=$(basename /run/kernel/*) && /usr/bin/seal-uki --target /run/target --output /out --secrets /run/secrets --kernel-dir "/run/kernel/${kver}" --seal-state sealed

FROM rootfs
COPY --from=sealed-uki /out/*.efi /boot/EFI/Linux/
