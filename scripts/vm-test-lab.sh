#!/usr/bin/env bash
# Local multi-OS KVM test lab for Lan Mouse (Linux <-> Windows 10 <-> Windows 11).
#
# Uses QEMU/KVM (and optionally `quickemu` / `quickget`) to spin up isolated
# Windows 10 and Windows 11 virtual machines on a shared network or generated
# config pair so you can test:
#   - Multi-monitor virtual displays (virtio-vga / qxl with multiple heads)
#   - Mixed DPI scale factors (e.g. 100% on Linux, 125% on Win10, 150% on Win11)
#   - Real edge transitions, low-level hooks, and clipboard sync
#
# Usage:
#   ./scripts/vm-test-lab.sh check
#   ./scripts/vm-test-lab.sh init [lab-dir]
#   ./scripts/vm-test-lab.sh gen-configs <host-ip> <win10-ip> [win11-ip]

set -euo pipefail

LAB_DIR="${LAB_DIR:-$HOME/lan-mouse-vms}"

usage() {
    cat <<'EOF'
Usage: ./scripts/vm-test-lab.sh <command> [args...]

Commands:
  check
      Check local KVM/QEMU, swtpm (for Windows 11 TPM 2.0), and quickemu status.

  init [lab-dir]
      Create the VM lab directory and generate ready-to-use quickemu configs
      (windows-10.conf and windows-11.conf) with SPICE/QXL multi-head support
      and a shared folder pointing to this repository's `dist/` directory.

  gen-configs <linux-ip> <win10-ip> [win11-ip]
      Generate pre-paired `config.toml` files (using `trusted_ips`) for the
      Linux host, Windows 10 VM, and optional Windows 11 VM so they trust each
      other on first connection without manual fingerprint prompts.
EOF
}

check_deps() {
    echo "==> Checking local virtualization capabilities..."
    if [[ -e /dev/kvm ]]; then
        echo "  [OK] /dev/kvm is present (hardware virtualization enabled)"
    else
        echo "  [WARN] /dev/kvm is missing; enable VT-x / AMD-V in BIOS"
    fi

    for bin in qemu-system-x86_64 swtpm quickemu quickget spicy; do
        if command -v "$bin" >/dev/null 2>&1; then
            echo "  [OK] $bin: $(command -v "$bin")"
        else
            echo "  [MISSING] $bin"
        fi
    done
}

init_lab() {
    local target_dir="${1:-$LAB_DIR}"
    local repo_root
    repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
    mkdir -p "$target_dir" "$repo_root/dist"

    cat >"$target_dir/windows-10.conf" <<EOF
#!/usr/bin/quickemu --vm
guest_os="windows"
disk_img="windows-10/disk.qcow2"
iso="windows-10/Win10.iso"
fixed_iso="windows-10/virtio-win.iso"
cpu_cores="4"
ram="8G"
secureboot="off"
tpm="off"
public_dir="$repo_root/dist"
# Enable 2 virtual heads on QXL/virtio so you can test multi-monitor DPI scaling
extra_args="-vga qxl -global qxl-vga.ram_size=134217728 -global qxl-vga.vram_size=134217728 -device virtio-serial-pci"
EOF

    cat >"$target_dir/windows-11.conf" <<EOF
#!/usr/bin/quickemu --vm
guest_os="windows"
disk_img="windows-11/disk.qcow2"
iso="windows-11/Win11.iso"
fixed_iso="windows-11/virtio-win.iso"
cpu_cores="4"
ram="8G"
secureboot="on"
tpm="on"
public_dir="$repo_root/dist"
# Enable 2 virtual heads on QXL/virtio so you can test multi-monitor DPI scaling
extra_args="-vga qxl -global qxl-vga.ram_size=134217728 -global qxl-vga.vram_size=134217728 -device virtio-serial-pci"
EOF

    chmod +x "$target_dir/windows-10.conf" "$target_dir/windows-11.conf"
    echo "==> Created VM configurations in $target_dir:"
    echo "    - $target_dir/windows-10.conf"
    echo "    - $target_dir/windows-11.conf"
    echo ""
    echo "Next steps:"
    echo "  1. Run: (cd \"$target_dir\" && quickget windows 10 && quickget windows 11)"
    echo "  2. Launch with bridge networking so host and VMs share a LAN:"
    echo "     quickemu --vm \"$target_dir/windows-10.conf\" --display spice"
    echo "     quickemu --vm \"$target_dir/windows-11.conf\" --display spice"
}

gen_configs() {
    if [[ $# -lt 2 ]]; then
        usage
        exit 1
    fi
    local linux_ip="$1"
    local win10_ip="$2"
    local win11_ip="${3:-}"
    local out_dir="${LAB_DIR}/configs"
    mkdir -p "$out_dir"

    local trusted="\"$linux_ip\", \"$win10_ip\""
    if [[ -n "$win11_ip" ]]; then
        trusted="$trusted, \"$win11_ip\""
    fi

    # 1. Linux Host config (Left: Win10, Right: Win11)
    cat >"$out_dir/linux-config.toml" <<EOF
port = 4242
trusted_ips = [$trusted]

[[clients]]
hostname = "win10-vm"
ips = ["$win10_ip"]
port = 4242
position = "left"
activate_on_startup = true
EOF
    if [[ -n "$win11_ip" ]]; then
        cat >>"$out_dir/linux-config.toml" <<EOF

[[clients]]
hostname = "win11-vm"
ips = ["$win11_ip"]
port = 4242
position = "right"
activate_on_startup = true
EOF
    fi

    # 2. Windows 10 VM config (Right: Linux, Top: Win11)
    cat >"$out_dir/win10-config.toml" <<EOF
port = 4242
trusted_ips = [$trusted]

[[clients]]
hostname = "linux-host"
ips = ["$linux_ip"]
port = 4242
position = "right"
activate_on_startup = true
EOF
    if [[ -n "$win11_ip" ]]; then
        cat >>"$out_dir/win10-config.toml" <<EOF

[[clients]]
hostname = "win11-vm"
ips = ["$win11_ip"]
port = 4242
position = "top"
activate_on_startup = true
EOF
    fi

    # 3. Windows 11 VM config (Left: Linux, Bottom: Win10)
    if [[ -n "$win11_ip" ]]; then
        cat >"$out_dir/win11-config.toml" <<EOF
port = 4242
trusted_ips = [$trusted]

[[clients]]
hostname = "linux-host"
ips = ["$linux_ip"]
port = 4242
position = "left"
activate_on_startup = true

[[clients]]
hostname = "win10-vm"
ips = ["$win10_ip"]
port = 4242
position = "bottom"
activate_on_startup = true
EOF
    fi

    echo "==> Generated pre-trusted configs in $out_dir:"
    ls -1 "$out_dir"
    echo "Copy win10-config.toml / win11-config.toml to %LOCALAPPDATA%\\lan-mouse\\config.toml inside each VM."
}

cmd="${1:-}"
shift || true

case "$cmd" in
    check)
        check_deps
        ;;
    init)
        init_lab "$@"
        ;;
    gen-configs)
        gen_configs "$@"
        ;;
    *)
        usage
        exit 1
        ;;
esac
