#!/usr/bin/env bash
# Línea de base: restore de snapshot de Firecracker en el mismo host.
#
# Arranca una microVM de Firecracker (kernel y rootfs de su CI), espera el
# prompt de login, la pausa, crea un snapshot completo y después mide N
# restores, cada uno en un proceso nuevo, con el backend de memoria `File`
# (MAP_PRIVATE del archivo de memoria, como documenta Firecracker):
#   - load_ms:  duración de PUT /snapshot/load con resume_vm=true (curl);
#   - spawn_ms: desde lanzar el proceso hasta que responde ese PUT.
# No mide cuándo el guest ejecuta su primera instrucción.
#
# Uso: tools/firecracker_baseline.sh SALIDA.json
# Variables: N (20), MEM_MIB (256), WORK (directorio temporal).
# Necesita /dev/kvm con lectura/escritura, curl, python3, unsquashfs,
# mkfs.ext4 y sudo (para armar el rootfs ext4).
set -euo pipefail

OUT=${1:?uso: $0 SALIDA.json}
OUT=$(realpath -m "$OUT")
N=${N:-20}
MEM_MIB=${MEM_MIB:-256}
FC_VERSION=v1.17.0
S3=https://s3.amazonaws.com/spec.ccfc.min
CI_PREFIX=firecracker-ci/20261028-84df49062576-0/x86_64
KERNEL=vmlinux-6.1.188
WORK=${WORK:-$(mktemp -d)}
cd "$WORK"

if [ ! -r /dev/kvm ] || [ ! -w /dev/kvm ]; then
    echo "no hay /dev/kvm con lectura/escritura" >&2
    exit 1
fi

curl -fsSL "https://github.com/firecracker-microvm/firecracker/releases/download/$FC_VERSION/firecracker-$FC_VERSION-x86_64.tgz" | tar -xz
FC="$WORK/release-$FC_VERSION-x86_64/firecracker-$FC_VERSION-x86_64"
"$FC" --version | head -n1
curl -fsSL -o vmlinux "$S3/$CI_PREFIX/$KERNEL"
curl -fsSL -o rootfs.squashfs "$S3/$CI_PREFIX/ubuntu-24.04.squashfs"
sudo unsquashfs -q -d root rootfs.squashfs >/dev/null
truncate -s 1G rootfs.ext4
sudo mkfs.ext4 -q -d root -F rootfs.ext4
sudo chown "$(id -u):$(id -g)" rootfs.ext4
sudo rm -rf root

api() { # api SOCK METHOD PATH JSON
    curl -fsS --unix-socket "$1" -X "$2" "http://localhost$3" \
        -H 'Content-Type: application/json' -d "$4" -o /dev/null -w '%{time_total}'
}
wait_sock() {
    for _ in $(seq 1 2000); do [ -S "$1" ] && return 0; sleep 0.005; done
    echo "el socket $1 no apareció" >&2; return 1
}

# Arranque y snapshot.
rm -f fc.sock
"$FC" --api-sock fc.sock > boot.log 2>&1 &
PID=$!
wait_sock fc.sock
api fc.sock PUT /boot-source \
    '{"kernel_image_path":"vmlinux","boot_args":"console=ttyS0 reboot=k panic=1 pci=off"}' >/dev/null
api fc.sock PUT /drives/rootfs \
    '{"drive_id":"rootfs","path_on_host":"rootfs.ext4","is_root_device":true,"is_read_only":false}' >/dev/null
api fc.sock PUT /machine-config "{\"vcpu_count\":1,\"mem_size_mib\":$MEM_MIB}" >/dev/null
BOOT_T0=$(date +%s%N)
api fc.sock PUT /actions '{"action_type":"InstanceStart"}' >/dev/null
for _ in $(seq 1 1200); do
    grep -q 'login:' boot.log && break
    kill -0 "$PID" 2>/dev/null || { cat boot.log; echo "firecracker terminó" >&2; exit 1; }
    sleep 0.1
done
grep -q 'login:' boot.log || { tail -n 40 boot.log; echo "el guest no llegó al login" >&2; exit 1; }
BOOT_MS=$(( ($(date +%s%N) - BOOT_T0) / 1000000 ))
api fc.sock PATCH /vm '{"state":"Paused"}' >/dev/null
SNAP_S=$(api fc.sock PUT /snapshot/create \
    '{"snapshot_type":"Full","snapshot_path":"snap.state","mem_file_path":"snap.mem"}')
kill "$PID"; wait "$PID" 2>/dev/null || true
ls -l snap.state snap.mem

# Restores.
: > samples.txt
for i in $(seq 1 "$N"); do
    rm -f r.sock
    T0=$(date +%s%N)
    "$FC" --api-sock r.sock > restore.log 2>&1 &
    PID=$!
    wait_sock r.sock
    LOAD_S=$(api r.sock PUT /snapshot/load \
        '{"snapshot_path":"snap.state","mem_backend":{"backend_path":"snap.mem","backend_type":"File"},"resume_vm":true}')
    T1=$(date +%s%N)
    kill -0 "$PID" || { cat restore.log; echo "el restore $i terminó el proceso" >&2; exit 1; }
    echo "$LOAD_S $(( (T1 - T0) / 1000 ))" >> samples.txt
    kill "$PID"; wait "$PID" 2>/dev/null || true
done

python3 - "$OUT" "$N" "$MEM_MIB" "$BOOT_MS" "$SNAP_S" "$FC_VERSION" "$KERNEL" <<'PY'
import json, math, sys
out, n, mem, boot_ms, snap_s, fcv, kernel = sys.argv[1:]
rows = [l.split() for l in open("samples.txt") if l.strip()]
load = [float(a) * 1000 for a, _ in rows]
spawn = [int(b) / 1000 for _, b in rows]
def pct(v, p):
    v = sorted(v); r = max(1, math.ceil(p / 100 * len(v)))
    return round(v[r - 1], 3)
res = {
    "firecracker": fcv, "guest_kernel": kernel, "mem_mib": int(mem), "n": len(rows),
    "boot_to_login_ms": int(boot_ms), "snapshot_create_ms": round(float(snap_s) * 1000, 3),
    "load_ms": {"p50": pct(load, 50), "p99": pct(load, 99), "max": round(max(load), 3)},
    "spawn_to_loaded_ms": {"p50": pct(spawn, 50), "p99": pct(spawn, 99), "max": round(max(spawn), 3)},
}
json.dump(res, open(out, "w"), indent=1)
print(json.dumps(res, indent=1))
PY
