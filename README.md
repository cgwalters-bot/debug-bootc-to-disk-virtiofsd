# bootc to-disk virtiofs diagnostics

This small project builds a diagnostic derivative image and runs `bcvk to-disk`
with a guest collector on `org.bootc.debug`. The collector emits bounded JSONL
records before SSH is allowed to become ready. It does not enable SSH; bcvk's
existing installer configuration does that.

## Use

Use an isolated bcvk checkout (for example a copy of `/tmp/opencode/bcvk-stock-v0.19.0`):

```sh
cp -a /tmp/opencode/bcvk-stock-v0.19.0 /tmp/bcvk-debug
sh patches/apply-bcvk.sh /tmp/bcvk-debug
cargo xtask build-image --base localhost/bootc-pr2290-a8df21a8-centos9-sealed-debugtools:debug
cargo xtask to-disk --image localhost/bootc-debug-virtiofsd:debug \
  --bcvk ~/.local/bin/bcvk --output /tmp/new-debug.qcow2
```

The command refuses existing disks and `/dev` targets, records metadata beside
the output, bounds the build/run, and requires a `ready` record in the capture.
Do not use plain Stream 9 as a reproducer for a composefs-derived workload:
`BASE_IMAGE` must identify the exact known derivative or original composefs
image under investigation. The image transaction intentionally does not update
the kernel, although package availability is reported by the build log.

The patch contains the safe outer FD-forwarding half of the bcvk 0.19 fix. The
remaining QEMU-config registration is intentionally not claimed here: the
stock 0.19 `run_ephemeral.rs` layout differs at that insertion point and an
unverified patch could silently put the device setup in the wrong scope. Until
that small second hunk is reviewed and added for the exact bcvk revision,
`cargo xtask to-disk` will fail loudly on the missing ready record rather than
reporting a successful empty capture. A host path alone is not safe because
QEMU runs inside the container boundary. The patch is deliberately not applied
to any checkout by this project.

The collector installs the source-checked probes documented in
`patches/probes-c9-abi.txt`, when tracefs, symbols, and permissions permit it.
It continuously drains nonblocking `/dev/kmsg` and `trace_pipe`. Probe traffic
is aggregated in a bounded userspace correlator; it emits three-second health
summaries, bounded oldest-unresolved request samples, and at most eight raw
negative-enqueue samples rather than serializing raw trace chunks. Health
records include collector-drop counters and per-CPU trace-buffer overrun stats.
An overrun makes request-lifecycle correlation inconclusive.
`ready` reports `probes_installed=complete|partial|none`,
`trace_reader_open=true|false`, and explicitly says `trace_lossless=false`.
Service readiness is never treated as proof of lossless tracing.

## Optional guest IRQ experiment

The image also contains the deliberately opt-in `guest-irq-test` helper. It is
disabled by default and only accepts the explicit `--guest-confirm` CLI when
the read-only QEMU fw_cfg opt-in is present. The collector can run the same
bounded worker when QEMU supplies `opt/bootc-debug/irq-mode` as exactly
`fixed` or `move`; it records original, effective/read-back, and restored
affinities in the `guest_irq` records. The helper only follows virtio devices
whose sysfs `device` ID is decimal/hex 26 (`VIRTIO_ID_FS`), resolves their PCI
ancestor and `msi_irqs`, and accepts only safe virtio-fs queue action names.
Network, block, HIPRIO, and config IRQs are excluded. It writes only the
documented `/proc/irq/<n>/smp_affinity_list` interface and restores originals.

Example (inside the disposable guest, not on the host; explicit CLI mode):

```text
guest-irq-test --mode move --duration-secs 120 --interval-ms 1000 --guest-confirm
```

The wrapper supplies it with
`-fw_cfg name=opt/bootc-debug/irq-mode,string=move`; do not add this as a
kernel argument because sealed UKIs reject that change. The optional systemd
unit is not enabled by the image and reads the fw_cfg mode directly.

Seal/build with the existing command (`cargo xtask build-image ...`); this
change adds the static musl helper alongside the collector. Do not rebuild the
image or run a VM until the coordinator supplies the wrapper and fw_cfg mode.

## Investigation handoff (verified 2026-09-10)

### Conclusion

The compelling root cause is QEMU commit `1ba9a522`: `SET_VRING_CALL` was
asynchronous even when `VHOST_USER_PROTOCOL_F_REPLY_ACK` was available. The
fix sets `VHOST_USER_NEED_REPLY_MASK` and waits for the backend reply before
testing pending interrupts. We did **not** capture the exact late eventfd
write, so this is an ordering/root-cause conclusion, not a claim about a
captured event.

The QEMU 10.1.5 baseline was source-compared with the patched build and had
one functional source difference: this patch. Source and binary hashes are
kept in `/var/home/sandbox-walters/src/qemu-ab/provenance.sha256`; the cached
builds are under `/var/home/sandbox-walters/src/qemu-ab` (`src-baseline`,
`src-patched`, and the trace variants). The patch is
`/var/home/sandbox-walters/src/qemu-ab/1ba9a522.patch`.

### Reproduction results

These are deliberately separate controls, not a combined success rate. The
natural-control ledger records B1 as a hang; records 6, P0, and 7 remain
separate under `target/runs/` and are not folded into the tables below.

| control | baseline | patched |
| --- | --- | --- |
| natural control B1 | hang | — |
| fixed guest IRQ affinity, B44 baseline / P45 patched | success | success |
| non-trace move, B46/B48/B50 | all three hang/stall | — |
| non-trace move, P47/P49/P51 | — | all three success |
| trace move, B54 | fail at 4m15s | — |
| trace move, P55 | — | success in 56s |

The fixed mode was forced through fw_cfg with `irqbalance` stopped (the unit
conflicted with the normal unit). The helper's actual minimum interval is one
second, not the earlier 100 ms description. It verified only PCI requests for
virtio device ID 26. The trace control remains useful: B54's
`qemu.trace` has flags `0x1` (read) and the patched run has `0x9`; the latter
shows read `0x5` before pending. Do not turn these small tables into a
grand statistical claim. Collector trace-buffer overruns and failed-early
setup runs are not evidence of the underlying behavior.

The vectors=0 compatibility experiment used older QEMU and succeeded three
times (79--81s), versus 56s for P55. It removes the guest MSI-X affinity
trigger, but startup mask events still occur (20 observed); it is not the
underlying fix. It is an experimental opt-in, not a default, and is not
implemented by a bcvk flag. Runtime version strings are not reliable enough
for backport autodetection.

### IRQ and kernel caveats

C9 is sensitive to managed IRQs. Linux commit `529395d2ae64` has managed IRQs
even with one queue; newer kernels can make the IRQ movable. C9 passes NULL
movable. A modern live check on Fedora 44 used kernel `7.1.12` and found
virtio-fs request IRQ 29, with a root write to
`/proc/irq/29/smp_affinity_list` failing `EPERM`; the root procfs was writable
but that IRQ was managed. Its artifact is
`target/runs/modern-irq-check/`. This has a different QEMU topology and does
not establish immunity.

### Packaging and CI status

The actual Ubuntu CI environment uses QEMU `9.2.1+ds-1ubuntu5`, verified in
[run 33818853327, job 100864735896](https://github.com/bootc-dev/bootc/actions/runs/33818853327/job/100864735896),
at action revision beginning `6e34`, with the Plucky setup. It is unfixed.
QEMU 10.1.3 **and 10.1.5 are also
unfixed**, contrary to the earlier agent claim; scans of the first 10.2
mainline backports vary, so do not infer status from a version alone.

The shipping local `actions/checkout` checkout is
`~/src/github/bootc-dev/actions` and has an uncommitted diff. The reviewed
workflow builds pinned QEMU 9.2.1 plus the upstream patch on Ubuntu 24.04
with Plucky packages, records source/patch/cache hashes, and uses native
architecture targets. It installs the disposable binary under `/usr/bin`
on the runner; it does not assume `QEMU_BIN`. Full Ubuntu x86 build,
cache/source/patch checks, ShellCheck, and actionlint passed and the reviewer
approved them. Native arm and a fresh full GHA workflow have **not** run.
The modified workflow checks both x86 and arm with `libvirt: true`.

The old artifact expired. A fresh CI run is needed for PR 2290; no external
push or PR was performed here. The relevant PR is
<https://github.com/bootc-dev/bootc/pull/2290>.

### Reproducible local handoff

The pinned CI QEMU binary is recorded by cache metadata as
`b52c216719570835d663993f6d39c9f5b6d0b74b419ebff6a57f9a1e37fb9cd6`;
the metadata also records source
`72874fe9c395ced0c7fd7c22c43744072697f7ee1926a72237bd81784b2faf62` and
patch `6da6ac4782d3e643abf94c23b39e8cd18a538ad12e83bbd083ed5adc033f467d`.
The installed static bcvk used for the older run has SHA-256
`a0cbdf346559da9dace6eaae4922d100a474169c3db8b87a4bd4f73ded25d455`.

Use variables instead of copying host-specific paths from run 61:

```sh
ROOT=$HOME/src/debug-bootc-to-disk-virtiofsd
QEMU=$HOME/src/qemu-ab/ci-runtime/qemu-system-x86_64
QEMU_DATA=$HOME/src/qemu-ab/ci-runtime/qemu-data-merged
BCVK=$HOME/.local/bin/bcvk
IMAGE=localhost/bootc-debug-virtiofsd:debug
cargo xtask to-disk --image "$IMAGE" --bcvk "$BCVK" \
  --qemu "$QEMU" --output "$ROOT/target/runs/new.qcow2"
```

For a direct ephemeral invocation, retain the run-61 shape: use bcvk's
`--qemu "$QEMU"`, `--ro-bind` the disposable log directory and the QEMU data
directory, and request serial capture. Bind the source `pc-bios` data path,
not a build-tree `pc-bios`; the Fedora portability wrapper otherwise needs
Ubuntu's `libaio`, data, and ROM paths. CI's Ubuntu-packaged data already has
the correct layout. Always use `--qemu`, `--ro-bind`, and serial capture
together; do not rely on a `QEMU_BIN` environment variable.

Runs 61 and 62 tested the same stress image with the patched CI 9.2.1 binary
and both succeeded (60s and 56s). This is **two runs**, not a 60/60 count.
Their artifacts are `target/runs/run61-patched-ci921-move/` and
`target/runs/run62-patched-ci921-move/`. Run 61's metadata/logs are the
authoritative commands and identities; substitute the variables above rather
than retaining its host paths.

The existing `xtask image`/harness documentation remains truthful: current
images require sealing after layers, and HOME-targeted runs can fail from
`/tmp` quota. Use a disposable output path and capture the console, journal,
virtio serial stream, and (when enabled) `qemu.trace`. The trace collector's
overrun counter is a limitation, not proof of a request failure.

### Next human actions

1. Review the uncommitted actions checkout and open a fresh PR 2290 CI run.
2. Confirm the run builds and caches QEMU 9.2.1 with the recorded source and
   patch hashes on x86; then run the native arm job.
3. Re-run one baseline and one patched stress case with the same image,
   QEMU topology, fw_cfg mode, and serial/trace capture. Preserve the run
   metadata and exclude early setup failures from behavioral counts.
4. Only after that, decide whether the QEMU patch belongs in the shipping
   bcvk path; keep vectors=0 as an explicitly experimental compatibility
   workaround.

### Source keys

* [QEMU `1ba9a522`](https://gitlab.com/qemu-project/qemu/-/commit/1ba9a522)
* [Linux `529395d2ae64`](https://git.kernel.org/pub/scm/linux/kernel/git/torvalds/linux.git/commit/?id=529395d2ae64)
* [bootc PR 2290](https://github.com/bootc-dev/bootc/pull/2290)
* `patches/probes-c9-abi.txt` — source-checked C9 probe ABI and kernel revision
* `target/runs/modern-irq-check/` — Fedora 44 managed-IRQ check
* `target/runs/run61-patched-ci921-move/` and `run62-patched-ci921-move/` — CI-QEMU local retests
