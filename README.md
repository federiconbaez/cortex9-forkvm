# cortex9-forkvm

[![CI](https://github.com/federiconbaez/cortex9-forkvm/actions/workflows/ci.yml/badge.svg)](https://github.com/federiconbaez/cortex9-forkvm/actions/workflows/ci.yml)
![License](https://img.shields.io/badge/license-Apache--2.0-blue)

Fork copy-on-write de microVMs calientes sobre KVM, en Rust: una VM plantilla corre hasta un punto de fork, se congela, y cada rama es una VM nueva que comparte la RAM de la plantilla página por página hasta que la escribe.

*Copy-on-write fork of warm microVMs on KVM, in Rust. English version below.*

---

## Español

### El problema

Un agente que explora (probar 8 parches, 8 comandos de migración) quiere clonar el entorno en el estado en que está y correr las ramas en paralelo, cada una con aislamiento de VM, en milisegundos y sin copiar toda la RAM.

### Qué hace

1. **Plantilla.** El VMM crea la RAM del guest como un `memfd` y la mapea `MAP_SHARED`. El guest (x86-64 en ensamblador, `guest/guest.S`) arranca directo en modo largo, llena un conjunto de trabajo "caliente" (64 MiB por defecto) con un patrón que depende de la dirección, deja marcadores en `r15`, `xmm0` y el MSR `KERNEL_GS_BASE`, y escribe en el puerto `0xf5`.
2. **Captura.** El VMM completa ese `OUT` con `KVM_RUN` e `immediate_exit = 1` (así RIP queda después de la instrucción sin ejecutar nada más), lee `regs`, `sregs`, FPU, XSAVE, XCRs, registros de depuración, eventos de la vCPU y una lista fija de MSRs (incluido el TSC), copia el estado de la UART, destruye la VM y **sella** el memfd (`F_SEAL_WRITE | F_SEAL_SHRINK | F_SEAL_GROW | F_SEAL_SEAL`). Sellado, el kernel rechaza cualquier escritura al memfd y cualquier mapeo compartido con escritura, así que la plantilla no puede cambiar.
3. **Fork.** Cada rama: `mmap(MAP_PRIVATE)` del memfd (las lecturas comparten las páginas de la plantilla; la primera escritura a una página la copia a memoria anónima de esa rama), `KVM_CREATE_VM` + vCPU + CPUID (o una VM vacía tomada de un pool), `KVM_SET_USER_MEMORY_REGION`, y el estado capturado. La rama arranca en la instrucción siguiente al punto de fork y lo primero que hace es leer su id de un puerto (`0xf6`): ese es el único dato que distingue a las ramas.
4. **Verificación.** Cada rama reescribe las primeras páginas del conjunto con un patrón propio, verifica en bucle que sigan siendo suyas, calcula el checksum de todo el conjunto y lo deja en un mailbox. El host compara contra una referencia calculada sin tocar la memoria del guest, recalcula el checksum leyendo la memoria de la rama, y comprueba que la plantilla siga con su checksum original.

Modos de `forkvm bench`:

| Modo | Memoria de la rama | Para qué |
|---|---|---|
| `cow` | `MAP_PRIVATE` del memfd sellado | lo que se mide |
| `cow --pool` | igual, con la VM y la vCPU creadas antes de largar | separar el costo de `KVM_CREATE_VM` |
| `copy` | copia completa de la RAM a memoria anónima | línea de base "copiar todo" |
| `shared` | `MAP_SHARED` del memfd sin sellar | **control negativo**: sin aislamiento; la verificación tiene que fallar |

### Alcance (lo que NO hace)

- Una sola vCPU, sin irqchip en el kernel (sin LAPIC, IOAPIC ni PIT), sin virtio (ni disco ni red) y sin interrupciones. El guest es un programa de prueba, no Linux. Con dispositivos reales habría que capturar y clonar su estado, y eso no está hecho.
- No hay re-seed de entropía ni ajuste de relojes después del fork. Cada rama recibe un id distinto por el puerto `0xf6` y el TSC sigue desde el valor de la plantilla; un sistema operativo de verdad necesitaría más (la documentación de Firecracker lo plantea como problema de seguridad, ver *Fuentes*).
- Sin live migration, sin snapshots en disco, sin memoria con huge pages.
- **La máquina de desarrollo no tiene acceso a `/dev/kvm`**. Ahí corren solo los tests unitarios, los de memoria del host y el guest emulado en Unicorn; las 8 pruebas de `tests/kvm.rs` fallan ahí a propósito. Toda la ejecución real sobre KVM y todas las mediciones son del job `kvm` de GitHub Actions, que primero exige `/dev/kvm` con lectura/escritura y falla si no está.

### Novedad, con honestidad

Baja. Firecracker ya carga snapshots creando un mapeo `MAP_PRIVATE` del archivo de memoria, con carga bajo demanda y copy-on-write a memoria anónima: es el mismo mecanismo de memoria. Lo distinto acá es acotado: el fork se hace en el mismo proceso desde una plantilla viva, sin escribir la RAM a un archivo, con el memfd sellado para que el kernel garantice que la plantilla no cambia, y con una verificación de aislamiento por checksums que tiene un control negativo. La idea original listaba otros productos de sandboxes (E2B, Modal, Daytona, microsandbox, Hyperlight, Cloud Hypervisor) que no revisé para este README, así que no afirmo nada sobre lo que hacen.

### Medición contra el criterio de aceptación

Criterio (idea #37): *fork → guest listo < 10 ms p50 por rama para 1, 8 y 64 ramas; RSS incremental por rama < 5 % de la RAM del guest; ninguna rama ve escrituras de otra (checksums); línea de base de restore de snapshot de Firecracker medida en el mismo host.*

Definiciones:

- **fork → lista**: desde que el hilo de la rama empieza el fork (incluye `mmap`, crear la VM salvo en `--pool`, memslot, estado) hasta que la rama ejecutó su primera instrucción de E/S. Las N ramas de una ronda hacen el fork al mismo tiempo, una por hilo.
- **Memoria por rama**: campo `Anonymous` de `/proc/self/smaps` del mapeo de la rama, leído con todas las ramas vivas después de su trabajo (escribir y verificar 256 páginas = 1 MiB durante 1 s, y leer los 64 MiB del conjunto). Es la memoria que la rama no comparte. Se informa también la caída de `MemAvailable` por rama, que incluye memoria del kernel (estructuras de KVM, EPT) y es ruidosa.
- RAM del guest: 256 MiB.

Comandos (los mismos que corre el job `kvm`):

```sh
cargo run --release -- bench --mode cow --branches 1,8,64 --rounds 20,5,2 --json cow-cold.json
cargo run --release -- bench --mode cow --pool --branches 1,8,64 --rounds 20,5,2 --json cow-pool.json
cargo run --release -- bench --mode copy --branches 1,8 --rounds 10,3 --json copy.json
cargo run --release -- bench --mode shared --branches 8 --rounds 1 --expect-violation --json shared.json
N=20 MEM_MIB=256 tools/firecracker_baseline.sh firecracker.json
```

**Resultados** del job `kvm` en el [run 40812805579](https://github.com/federiconbaez/cortex9-forkvm/actions/runs/40812805579) sobre `main` (`8f307be`): runner `ubuntu-latest` con Intel Xeon Platinum 8573C, 4 vCPU, 16 GiB y kernel 7.0.0-1017-azure (KVM anidado dentro de la VM del runner). Plantilla: arranque → punto de fork 118 ms (incluye llenar 64 MiB), captura 8.5 ms.

Fork → rama lista, por rama (ms):

| N ramas | Rondas / muestras | Cáscara en el fork: p50 | p99 | Cáscara de un pool: p50 | p99 |
|---|---|---|---|---|---|
| 1 | 20 / 20 | 0.831 | 1.279 | 0.275 | 0.314 |
| 8 | 5 / 40 | 0.955 | 1.925 | 0.450 | 1.028 |
| 64 | 2 / 128 | **10.469** | 15.885 | 1.825 | 9.006 |

Memoria y aislamiento (mismo run):

| | Cáscara en el fork | Pool | Copia completa (`copy`) | Control `shared` |
|---|---|---|---|---|
| Memoria anónima por rama, p50 = máx | 1044 KiB (**0.40 %** de 256 MiB) | 1044 KiB (0.40 %) | 262144 KiB (100 %) | 0 KiB (todas escriben en el memfd) |
| Ramas que pasan la verificación | 188/188 | 188/188 | 34/34 | **0/8** (detectado) |
| Plantilla intacta después de cada ronda | 27/27 | 27/27 | 13/13 | 0/1 (detectado) |
| Fork → lista p50, N=1 / N=8 | (arriba) | (arriba) | 147.5 / 366.1 ms | — |

Los 1044 KiB son las 256 páginas que escribe cada rama (1024 KiB) más 5 páginas de pila, mailbox y tablas de páginas del guest (el CPU marca bits A/D). La caída de `MemAvailable` por rama osciló entre 0 y 0.9 MiB y `SecPageTables` (EPT) creció entre 16 y 138 KiB por rama; las dos incluyen memoria del kernel y se publican solo como referencia.

Línea de base de Firecracker en el mismo runner y el mismo job (v1.17.0, guest Linux 6.1.188 con 256 MiB y 1 vCPU, backend `File`, 20 restores): `PUT /snapshot/load` con `resume_vm` p50 **3.159 ms**, p99 4.297 ms; desde lanzar el proceso hasta esa respuesta p50 10.423 ms, p99 15.323 ms. Crear el snapshot completo tardó 1482 ms y el arranque hasta el login 1862 ms.

Veredicto contra el criterio:

- **< 10 ms p50 por rama para 1, 8 y 64 ramas: no se cumple con la VM creada dentro del fork** (10.469 ms con 64 ramas) y **sí con VMs vacías de un pool** (1.825 ms). En la ronda de 64 lo que crece es el primer `KVM_RUN` (4.656 ms p50) y el `mmap` (1.744 ms p50), no `KVM_CREATE_VM` (0.290 ms p50); lo atribuyo a 64 hilos compitiendo por 4 vCPU y por el lock de mapeos del proceso, pero no lo medí por separado.
- **Memoria por rama < 5 %: se cumple** (0.40 %); la copia completa usa el 100 %.
- **Aislamiento: se cumple** en las 376 ramas CoW medidas, con la plantilla intacta en todas las rondas, y el control `MAP_SHARED` se detecta en 8 de 8 ramas.
- **Línea de base de Firecracker: medida**. Su `load` (3.2 ms) es del mismo orden que el fork en frío de forkvm con 1 u 8 ramas (0.8 – 1.0 ms); la diferencia grande está en crear el snapshot (1482 ms escribiendo la RAM a un archivo contra 8.5 ms de sellar el memfd). No es una comparación pareja: Firecracker restaura un Linux completo con dispositivos en un proceso nuevo.

En un segundo runner (AMD EPYC 7763, [run 40812522531](https://github.com/federiconbaez/cortex9-forkvm/actions/runs/40812522531) del PR #1, mismo código) los números fueron parecidos salvo N=64 en frío, peor: p50 16.572 ms (pool: 3.147 ms); Firecracker `load` p50 3.183 ms.

### Línea de base de Firecracker

`tools/firecracker_baseline.sh` baja Firecracker v1.17.0 y el kernel `vmlinux-6.1.188` y el rootfs Ubuntu 24.04 del CI de Firecracker, arranca una microVM de 256 MiB y 1 vCPU, espera el prompt de login, la pausa, crea un snapshot completo y mide 20 restores en procesos nuevos con el backend `File`. Mide la duración de `PUT /snapshot/load` con `resume_vm: true` y el tiempo desde lanzar el proceso hasta esa respuesta. No mide cuándo el guest ejecuta su primera instrucción, y la comparación no es pareja: Firecracker restaura un Linux completo con dispositivos en un proceso nuevo, forkvm clona un guest mínimo dentro del mismo proceso.

### Tests

- `cargo test --lib --bins --test host`: 42 unitarios (layout, modo largo, UART, patrón y checksum del guest, parseo de smaps/meminfo, filtro de MSRs, validación, percentiles) y 6 de memoria en el host sin KVM (`MAP_PRIVATE` es copy-on-write, una plantilla sin sellar se filtra a las páginas no copiadas, el sello bloquea `mmap` compartido, `pwrite` y `ftruncate`, smaps cuenta como anónimas solo las páginas copiadas, dos copias anónimas vecinas se cuentan por separado).
- `tools/emulate_fork.py`: el guest emulado en Unicorn, plantilla + 4 ramas contra checksums calculados en Python, y un control que altera un qword y tiene que detectarse.
- `cargo test --release --test kvm -- --test-threads=1`: 8 pruebas sobre KVM real (RIP en `branch_entry` después de `immediate_exit`, marcadores de r15/xmm0/MSR/TSC, 8 ramas CoW aisladas con 64 páginas copiadas cada una más un margen chico, pool, rondas repetidas, copia completa, plantilla sellada que rechaza `MAP_SHARED` y control negativo con `MAP_SHARED`). Fallan si no hay `/dev/kvm`.
- En el run 40812805579: 42/42, 6/6 y 8/8 (56/56), más las dos corridas del guest emulado.

### Uso

```sh
cargo build --release
./target/release/forkvm boot                    # arranca la plantilla e imprime el estado
./target/release/forkvm bench --branches 8 --rounds 3 --work-ms 200
```

Requiere Linux x86-64, `/dev/kvm` con lectura/escritura y binutils (`as`, `ld`, `objcopy`, `nm`) para ensamblar el guest.

### Fuentes que leí para esto

- Firecracker, [Snapshotting support](https://github.com/firecracker-microvm/firecracker/blob/main/docs/snapshotting/snapshot-support.md): carga con `MAP_PRIVATE` del archivo de memoria, API de pausa, creación y carga, backend `File`, y la sección sobre unicidad al restaurar el mismo snapshot más de una vez.
- Firecracker, [Getting started](https://github.com/firecracker-microvm/firecracker/blob/main/docs/getting-started.md): de dónde bajar el binario, el kernel y el rootfs de su CI.
- Linux, [Documentation/virt/kvm/api.rst](https://github.com/torvalds/linux/blob/master/Documentation/virt/kvm/api.rst), `immediate_exit` y la nota de `KVM_RUN` sobre completar `KVM_EXIT_IO` antes de leer el estado.
- [mmap(2)](https://man7.org/linux/man-pages/man2/mmap.2.html), `MAP_PRIVATE` (no especifica si los cambios posteriores al archivo se ven en el mapeo: por eso se sella).
- [F_ADD_SEALS(2const)](https://man7.org/linux/man-pages/man2/F_ADD_SEALS.2const.html), `F_SEAL_WRITE` y el `EBUSY` si queda un mapeo compartido con escritura.

El VMM base (modo largo, tablas de páginas, UART 16550) viene de un VMM mínimo propio y se copió acá (`src/layout.rs`, `src/boot.rs`, `src/uart.rs`).

---

## English

**What it is.** A template VM runs on a `memfd`-backed RAM up to a fork point (an `OUT` to port `0xf5`). The VMM completes that `OUT` with `KVM_RUN` + `immediate_exit = 1`, captures regs, sregs, FPU, XSAVE, XCRs, debug registers, vCPU events and a fixed MSR list (TSC included) plus the UART state, destroys the VM and seals the memfd (`F_SEAL_WRITE|SHRINK|GROW|SEAL`), so the kernel refuses any write to the template. Each branch is a new KVM VM whose RAM is a `MAP_PRIVATE` mapping of that memfd (reads share the template pages, the first write to a page copies it), with the captured state restored. The only thing that tells branches apart is an id the guest reads from port `0xf6`.

**Isolation check.** Every branch rewrites the first pages of the warm set with its own pattern, keeps verifying them, checksums the whole set and reports it. The host compares against a reference computed without touching guest memory, recomputes the checksum from the branch's memory, and checks the template checksum is unchanged. A `shared` mode (`MAP_SHARED`, unsealed) is a negative control that must be detected.

**Scope.** One vCPU, no in-kernel irqchip, no virtio, no interrupts; the guest is a test program, not Linux. No entropy reseeding or clock fix-up after fork. **The development machine has no `/dev/kvm` access**: only unit tests, host memory tests and the Unicorn-emulated guest run there, and the 8 KVM tests fail there on purpose. Every real KVM run and every number comes from the `kvm` GitHub Actions job, which requires a read/write `/dev/kvm` and fails otherwise.

**Novelty.** Low. Firecracker already restores snapshots with a `MAP_PRIVATE` mapping of the memory file (same memory mechanism). What differs is narrow: in-process fork from a live template with no memory file, a sealed memfd, and a checksum isolation test with a negative control. Other sandbox products named in the original idea were not reviewed for this README.

**Acceptance criterion** (idea #37): fork → guest ready < 10 ms p50 per branch for 1, 8 and 64 branches; incremental RSS per branch < 5 % of guest RAM (measured as the branch mapping's `Anonymous` in smaps); no branch sees another's writes; a Firecracker snapshot-restore baseline measured on the same host. Results from [run 40812805579](https://github.com/federiconbaez/cortex9-forkvm/actions/runs/40812805579) on `main` (Intel Xeon Platinum 8573C, 4 vCPU, kernel 7.0.0-1017-azure, nested KVM): fork → ready p50 per branch with the VM created inside the fork 0.831 / 0.955 / **10.469 ms** for 1 / 8 / 64 branches, and with pre-created empty VMs 0.275 / 0.450 / 1.825 ms. Anonymous memory per branch 1044 KiB (0.40 % of 256 MiB) versus 256 MiB for the full-copy baseline (147.5 ms p50 per fork). 376 of 376 CoW branches verified with the template intact; the `MAP_SHARED` control was detected in 8 of 8 branches. Firecracker on the same runner: `PUT /snapshot/load` p50 3.159 ms, process start to loaded p50 10.423 ms, full snapshot creation 1482 ms (forkvm capture: 8.5 ms). Verdict: the < 10 ms p50 target **is not met at 64 branches when the VM is created inside the fork** (CPU contention with 64 threads on 4 vCPUs) and is met with a pool (the growth at 64 is in the first `KVM_RUN` and `mmap`, which I attribute to CPU and mapping-lock contention without having measured it separately); the memory and isolation targets are met. Nothing was measured on the development machine.

**Firecracker baseline.** `tools/firecracker_baseline.sh` uses Firecracker v1.17.0 with the kernel and Ubuntu rootfs from Firecracker's CI, 256 MiB and 1 vCPU, and times `PUT /snapshot/load` (`File` backend, `resume_vm: true`) over 20 fresh processes. It is not an apples-to-apples comparison: full Linux with devices in a new process versus a minimal guest cloned in-process.

**Sources actually read** are listed in the Spanish section (Firecracker snapshot and getting-started docs, the KVM API doc, `mmap(2)` and `F_ADD_SEALS(2const)`).

## Licencia / License

Apache-2.0. Ver [LICENSE](LICENSE) y [NOTICE](NOTICE).
