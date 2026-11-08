<div align="center">

# 💾 DriveSwap

### Back up a whole drive to `C:` → format & repair it → reinstall Ventoy → restore everything exactly → free the space.
**One command. Resumable at every step. Written in Rust.**

![platform](https://img.shields.io/badge/platform-Windows%2010%2F11-0078D6?logo=windows)
![language](https://img.shields.io/badge/rust-1.70%2B-orange?logo=rust)
![license](https://img.shields.io/badge/license-MIT-green)

</div>

---

## Why

A drive acts up — Windows says *"You need to format the disk"*, `chkdsk` crashes, the volume won't mount — but it holds
a terabyte of data. DriveSwap automates the only reliable cure: **evacuate → reformat → put everything back**, as fast as the
hardware allows, without losing a single healthy file.

## What it does

| Step | What happens |
|------|--------------|
| **1 · Backup** | Copies every file and folder to `C:\DriveSwap\<L>\data` with 16 parallel threads and 16 MB buffers (biggest files first). Files and folders that are **corrupted / unreadable are skipped** and listed in `skipped.txt`. If Windows cannot even mount the volume, the built-in **raw exFAT reader** reads it straight from the disk. The backup is verified before anything else happens. |
| **2 · Format** | Re-creates the file system with the **same type, label and cluster size** (10 s abort window unless `--yes`). Refuses to run unless step 1 is verified. |
| **2b · Ventoy** | If the drive was a [Ventoy](https://www.ventoy.net) drive (label `Ventoy` / `VTOYEFI` partition) or you pass `--ventoy`, the latest official Ventoy is downloaded once and installed (`/U` update, falling back to a fresh `/I` install). A Ventoy failure never blocks the restore. |
| **3 · Restore** | Copies everything back with the original **timestamps and attributes** (hidden / read-only / system), recreating empty folders too, then verifies size + time of every file (`--verify` adds a full BLAKE3 content check). |
| **4 · Cleanup** | Deletes `C:\DriveSwap\<L>` completely, in parallel, and reports the space freed. Refuses unless the restore was verified. |

### Resumable by design
Progress is checkpointed in `C:\DriveSwap\<L>\state.json`. **Run it again at any time — it skips every step that is already done** and continues with the next one (a half-finished copy resumes where it stopped; files already copied are not re-copied).

## Usage

```powershell
driveswap.exe            # asks for the drive letter (default E)
driveswap.exe E          # do everything for drive E:
driveswap.exe E --yes    # no 10-second pause before formatting
driveswap.exe E --verify # byte-level BLAKE3 verification after restore
driveswap.exe E --raw    # force the raw exFAT reader (unmountable drive)
driveswap.exe E --status # show saved progress
driveswap.exe E --stop-after backup   # run only up to a step (backup|format|restore)
```

It asks for Administrator rights itself (UAC prompt) — raw disk access and formatting need them.
It will **never** touch the system drive.

## Speed

NVMe → NVMe I/O is the limit: ~1.5 GB/s measured with the parallel copier, so roughly **11 minutes per TB, each way**.
A 1.5 TB drive: ≈ 20 min backup + ≈ 20 min restore + ~1 min format/Ventoy + ~1–2 min cleanup.
Re-running after the backup is done skips straight to the format.

## Safety rules baked in

- Source drive is **read-only** during backup (the raw reader opens the disk read-only).
- Never formats unless the backup is complete **and** verified.
- Never deletes the backup unless the restore is complete **and** verified.
- Copies go to `*.dswp` temp files and are renamed on success, so an interrupted run never leaves a half-written file that looks complete.
- Skipped items are written to `C:\DriveSwap\skipped_<L>.txt` so you know exactly what was corrupted.

## Limits (honest)

- exFAT stores times on a **2-second grid**, so a timestamp can come back up to 2 s later than the original. File *content* is byte-identical.
- Raw rescue mode supports **exFAT** volumes. NTFS / FAT32 drives must be mountable.
- Creation / access times are not preserved, only last-write time and attributes.
- The Ventoy step needs internet the first time (downloads the official release from GitHub) and a drive layout Ventoy accepts.
- Make sure `C:` has free space for the data you want to keep (checked up front).

## Build

```powershell
cargo build --release          # produces target\release\driveswap.exe  (GNU or MSVC toolchain)
```

Dependencies: `serde`, `serde_json`, `walkdir`, `blake3`. Everything else is `std` + a few Win32 calls.

## How the raw reader works

When Windows refuses to mount a volume (e.g. corrupt exFAT *up-case table* — the original reason this tool exists), DriveSwap parses the exFAT boot sector, FAT and directory entry sets itself, validates every entry-set checksum, drops entries that fail, and streams file data directly from the disk. Damaged directories are reported and skipped instead of aborting the run.

## License
MIT
