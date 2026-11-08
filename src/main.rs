//! driveswap — back up a whole drive to C:, format/repair it, (re)install Ventoy,
//! restore everything exactly, then delete the backup and free the space.
//! Every stage is checkpointed: re-running resumes at the next unfinished stage.
mod exfat;

use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::fs::{self, File};
use std::io::{self, Read, Write};
use std::os::windows::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering::Relaxed};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

#[link(name = "kernel32")]
extern "system" {
    fn SetFileAttributesW(p: *const u16, a: u32) -> i32;
    fn CreateFileW(name: *const u16, access: u32, share: u32, sec: *mut u8, disp: u32, flags: u32, tmpl: *mut u8) -> isize;
    fn SetFileTime(h: isize, c: *const u64, a: *const u64, w: *const u64) -> i32;
    fn CloseHandle(h: isize) -> i32;
    fn GetDiskFreeSpaceExW(p: *const u16, avail: *mut u64, total: *mut u64, free: *mut u64) -> i32;
}

const THREADS: usize = 16;
const CHUNK: usize = 16 * 1024 * 1024;
const TMP_EXT: &str = ".dswp";

fn wide(p: &Path) -> Vec<u16> {
    use std::os::windows::ffi::OsStrExt;
    p.as_os_str().encode_wide().chain(Some(0)).collect()
}
fn set_attrs(p: &Path, a: u32) {
    let m = a & 0x27;
    unsafe { SetFileAttributesW(wide(p).as_ptr(), if m == 0 { 0x80 } else { m }) };
}
fn free_space(p: &Path) -> u64 {
    let (mut a, mut t, mut f) = (0u64, 0u64, 0u64);
    unsafe { GetDiskFreeSpaceExW(wide(p).as_ptr(), &mut a, &mut t, &mut f) };
    f
}

// ───────────────────────────── state ─────────────────────────────
#[derive(Serialize, Deserialize, Clone)]
struct FileRec {
    rel: String,
    size: u64,
    mtime: i64,
    mtime_ns: u32,
    attrs: u32,
}
#[derive(Serialize, Deserialize, Clone)]
struct DirRec {
    rel: String,
    mtime: i64,
    mtime_ns: u32,
    attrs: u32,
}
#[derive(Serialize, Deserialize, Default)]
struct State {
    drive: String,
    disk: u32,
    part_off: u64,
    fs: String,
    label: String,
    cluster: u64,
    ventoy: bool,
    backup_done: bool,
    format_done: bool,
    ventoy_done: bool,
    restore_done: bool,
    bytes: u64,
    files: Vec<FileRec>,
    dirs: Vec<DirRec>,
    skipped: Vec<String>,
}

struct Ctx {
    letter: char,
    work: PathBuf,
    yes: bool,
    verify: bool,
    force_raw: bool,
    ventoy_flag: bool,
}
impl Ctx {
    fn root(&self) -> PathBuf {
        PathBuf::from(format!("{}:\\", self.letter))
    }
    fn data(&self) -> PathBuf {
        self.work.join("data")
    }
    fn state_path(&self) -> PathBuf {
        self.work.join("state.json")
    }
}

fn save(c: &Ctx, s: &State) -> Result<(), String> {
    fs::create_dir_all(&c.work).map_err(|e| e.to_string())?;
    let tmp = c.work.join("state.json.tmp");
    fs::write(&tmp, serde_json::to_vec(s).map_err(|e| e.to_string())?).map_err(|e| e.to_string())?;
    fs::rename(&tmp, c.state_path()).map_err(|e| e.to_string())
}

// ───────────────────────────── helpers ─────────────────────────────
fn ps(script: &str) -> Result<String, String> {
    let o = Command::new("powershell")
        .args(["-NoProfile", "-NonInteractive", "-ExecutionPolicy", "Bypass", "-Command", script])
        .output()
        .map_err(|e| e.to_string())?;
    let out = String::from_utf8_lossy(&o.stdout).trim().to_string();
    if o.status.success() {
        Ok(out)
    } else {
        Err(format!("{} {}", out, String::from_utf8_lossy(&o.stderr).trim()))
    }
}

fn is_admin() -> bool {
    Command::new("fltmc").output().map(|o| o.status.success()).unwrap_or(false)
}

fn unix_of(t: SystemTime) -> (i64, u32) {
    match t.duration_since(UNIX_EPOCH) {
        Ok(d) => (d.as_secs() as i64, d.subsec_nanos()),
        Err(e) => (-(e.duration().as_secs() as i64), 0),
    }
}
/// Set last-write time on a file OR directory (needs only FILE_WRITE_ATTRIBUTES, works on read-only files).
fn set_mtime(p: &Path, secs: i64, ns: u32) {
    let ft = ((secs + 11_644_473_600) as u64) * 10_000_000 + (ns / 100) as u64;
    unsafe {
        let h = CreateFileW(wide(p).as_ptr(), 0x100, 7, std::ptr::null_mut(), 3, 0x0200_0000, std::ptr::null_mut());
        if h != -1 {
            SetFileTime(h, std::ptr::null(), std::ptr::null(), &ft);
            CloseHandle(h);
        }
    }
}
fn gb(b: u64) -> f64 {
    b as f64 / 1e9
}
fn say(s: &str) {
    println!("{}", s);
    let _ = io::stdout().flush();
}
fn is_sysname(n: &str) -> bool {
    n.eq_ignore_ascii_case("System Volume Information") || n.eq_ignore_ascii_case("$RECYCLE.BIN")
}
fn join_rel(base: &str, name: &str) -> String {
    if base.is_empty() {
        name.to_string()
    } else {
        format!("{}\\{}", base, name)
    }
}

// progress reporter
struct Prog {
    bytes: AtomicU64,
    files: AtomicU64,
    total_bytes: u64,
    total_files: u64,
    stop: AtomicBool,
}
fn start_reporter(p: Arc<Prog>, title: &'static str) -> std::thread::JoinHandle<()> {
    std::thread::spawn(move || {
        let t0 = Instant::now();
        while !p.stop.load(Relaxed) {
            std::thread::sleep(Duration::from_millis(1000));
            let b = p.bytes.load(Relaxed);
            let el = t0.elapsed().as_secs_f64().max(0.001);
            let rate = b as f64 / el;
            let eta = if rate > 1.0 { (p.total_bytes.saturating_sub(b)) as f64 / rate } else { 0.0 };
            print!(
                "\r  {title}: {:.1}/{:.1} GB  {}/{} files  {:.0} MB/s  ETA {:.0}m{:02.0}s   ",
                gb(b),
                gb(p.total_bytes),
                p.files.load(Relaxed),
                p.total_files,
                rate / 1e6,
                (eta / 60.0).floor(),
                eta % 60.0
            );
            let _ = io::stdout().flush();
        }
        println!();
    })
}

fn run_pool<F: Fn(usize) + Sync>(n: usize, f: F) {
    let next = AtomicUsize::new(0);
    std::thread::scope(|s| {
        for _ in 0..THREADS.min(n.max(1)) {
            s.spawn(|| loop {
                let i = next.fetch_add(1, Relaxed);
                if i >= n {
                    break;
                }
                f(i);
            });
        }
    });
}

// ───────────────────────────── drive info ─────────────────────────────
fn drive_info(c: &Ctx) -> Result<State, String> {
    let l = c.letter;
    let out = ps(&format!(
        "$p=Get-Partition -DriveLetter {l}; $v=Get-Volume -DriveLetter {l} -ErrorAction SilentlyContinue; \
         $ve=$false; Get-Partition -DiskNumber $p.DiskNumber | ForEach-Object {{ $x=Get-Volume -Partition $_ -ErrorAction SilentlyContinue; if($x -and $x.FileSystemLabel -eq 'VTOYEFI'){{$ve=$true}} }}; \
         '{{0}}|{{1}}|{{2}}|{{3}}|{{4}}|{{5}}' -f $p.DiskNumber,$p.Offset,$v.FileSystem,$v.FileSystemLabel,$v.AllocationUnitSize,$ve"
    ))?;
    let f: Vec<&str> = out.lines().last().unwrap_or("").split('|').collect();
    if f.len() < 6 {
        return Err(format!("cannot read drive info: {}", out));
    }
    let mut s = State { drive: l.to_string(), ..Default::default() };
    s.disk = f[0].parse().map_err(|_| "bad disk number")?;
    s.part_off = f[1].parse().map_err(|_| "bad partition offset")?;
    s.fs = f[2].to_string();
    s.label = f[3].to_string();
    s.cluster = f[4].parse().unwrap_or(0);
    s.ventoy = c.ventoy_flag || f[5].eq_ignore_ascii_case("true") || s.label.eq_ignore_ascii_case("ventoy");
    if s.fs.is_empty() || s.fs.eq_ignore_ascii_case("unknown") || s.cluster == 0 {
        // unmounted: read the boot sector directly
        if let Ok(v) = exfat::Vol::open(s.disk, s.part_off) {
            s.fs = "exFAT".into();
            s.cluster = v.csz;
            if s.label.is_empty() {
                s.label = v.label();
            }
            if s.label.eq_ignore_ascii_case("ventoy") {
                s.ventoy = true;
            }
        } else if s.fs.is_empty() {
            return Err("volume is not mounted and is not exFAT — cannot read it".into());
        }
    }
    if s.ventoy && s.label.is_empty() {
        s.label = "Ventoy".into();
    }
    Ok(s)
}

// ───────────────────────────── scanning ─────────────────────────────
#[derive(Clone)]
enum Src {
    Path(PathBuf),
    Raw { fc: u32, nofat: bool },
}
struct SrcFile {
    rec: FileRec,
    src: Src,
}

fn scan_mounted(root: &Path, files: &mut Vec<SrcFile>, dirs: &mut Vec<DirRec>, skipped: &mut Vec<String>) {
    let w = walkdir::WalkDir::new(root)
        .min_depth(1)
        .follow_links(false)
        .into_iter()
        .filter_entry(|e| !(e.depth() == 1 && is_sysname(&e.file_name().to_string_lossy())));
    for e in w {
        let e = match e {
            Ok(e) => e,
            Err(er) => {
                skipped.push(format!("{} : {}", er.path().map(|p| p.display().to_string()).unwrap_or_default(), er));
                continue;
            }
        };
        let rel = e.path().strip_prefix(root).unwrap().to_string_lossy().to_string();
        let md = match e.metadata() {
            Ok(m) => m,
            Err(er) => {
                skipped.push(format!("{} : {}", rel, er));
                continue;
            }
        };
        let (mt, ns) = md.modified().map(unix_of).unwrap_or((0, 0));
        let attrs = md.file_attributes();
        if attrs & 0x400 != 0 {
            skipped.push(format!("{} : reparse point/symlink skipped", rel));
            continue;
        }
        if md.is_dir() {
            dirs.push(DirRec { rel, mtime: mt, mtime_ns: ns, attrs });
        } else if md.is_file() {
            files.push(SrcFile {
                rec: FileRec { rel, size: md.len(), mtime: mt, mtime_ns: ns, attrs },
                src: Src::Path(e.path().to_path_buf()),
            });
        }
    }
}

fn scan_raw(v: &exfat::Vol, fc: u32, size: u64, nofat: bool, base: &str, files: &mut Vec<SrcFile>, dirs: &mut Vec<DirRec>, skipped: &mut Vec<String>) {
    let items = match v.read_dir(fc, size, nofat) {
        Ok(i) => i,
        Err(e) => {
            skipped.push(format!("{} : unreadable directory ({})", if base.is_empty() { "\\" } else { base }, e));
            return;
        }
    };
    for it in items {
        let e = match it {
            exfat::Item::Ok(e) => e,
            exfat::Item::Bad => {
                skipped.push(format!("{} : corrupted directory entry", if base.is_empty() { "\\" } else { base }));
                continue;
            }
        };
        if base.is_empty() && is_sysname(&e.name) {
            continue;
        }
        let rel = join_rel(base, &sanitize(&e.name));
        if e.is_dir {
            dirs.push(DirRec { rel: rel.clone(), mtime: e.mtime, mtime_ns: e.mtime_ns, attrs: e.attr });
            scan_raw(v, e.fc, e.size, e.nofat, &rel, files, dirs, skipped);
        } else {
            if e.size > 0 {
                if let Err(er) = v.chain(e.fc, e.size, e.nofat) {
                    skipped.push(format!("{} : {}", rel, er));
                    continue;
                }
            }
            files.push(SrcFile {
                rec: FileRec { rel, size: e.size, mtime: e.mtime, mtime_ns: e.mtime_ns, attrs: e.attr },
                src: Src::Raw { fc: e.fc, nofat: e.nofat },
            });
        }
    }
}

fn sanitize(n: &str) -> String {
    let s: String = n.chars().map(|c| if "<>:\"|?*\\/".contains(c) || (c as u32) < 32 { '_' } else { c }).collect();
    let t = s.trim_end_matches(|c| c == ' ' || c == '.');
    if t.is_empty() { "_".into() } else { t.to_string() }
}

// ───────────────────────────── file copy ─────────────────────────────
fn tmp_of(p: &Path) -> PathBuf {
    let mut s = p.as_os_str().to_os_string();
    s.push(TMP_EXT);
    PathBuf::from(s)
}

fn copy_file(src: &Src, vol: Option<&exfat::Vol>, size: u64, dst: &Path, prog: &Prog) -> io::Result<()> {
    let tmp = tmp_of(dst);
    let r = (|| -> io::Result<()> {
        let mut out = File::create(&tmp)?;
        if size > 0 {
            let _ = out.set_len(size);
        }
        let mut buf = vec![0u8; CHUNK];
        match src {
            Src::Path(p) => {
                let mut f = File::open(p)?;
                let mut left = size;
                while left > 0 {
                    let n = (left as usize).min(CHUNK);
                    f.read_exact(&mut buf[..n])?;
                    out.write_all(&buf[..n])?;
                    left -= n as u64;
                    prog.bytes.fetch_add(n as u64, Relaxed);
                }
            }
            Src::Raw { fc, nofat } => {
                let v = vol.unwrap();
                let cl = v.chain(*fc, size, *nofat).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
                let per = (CHUNK as u64 / v.csz).max(1) as usize;
                let mut left = size;
                let mut i = 0;
                while i < cl.len() {
                    let mut j = i;
                    while j + 1 < cl.len() && cl[j + 1] == cl[j] + 1 && (j + 1 - i) < per {
                        j += 1;
                    }
                    let nbytes = (j + 1 - i) as u64 * v.csz;
                    v.read_at(v.cluster_off(cl[i]), &mut buf[..nbytes as usize])?;
                    let w = nbytes.min(left) as usize;
                    out.write_all(&buf[..w])?;
                    left -= w as u64;
                    prog.bytes.fetch_add(w as u64, Relaxed);
                    i = j + 1;
                }
            }
        }
        out.flush()?;
        Ok(())
    })();
    match r {
        Ok(()) => {
            let _ = fs::remove_file(dst);
            fs::rename(&tmp, dst)
        }
        Err(e) => {
            let _ = fs::remove_file(&tmp);
            Err(e)
        }
    }
}

/// Copy a list of files in parallel (largest first). Returns indices that failed.
fn copy_all(items: &[(Src, FileRec)], dst_root: &Path, vol: Option<&exfat::Vol>, apply_attrs: bool, title: &'static str) -> Vec<(usize, String)> {
    let total: u64 = items.iter().map(|i| i.1.size).sum();
    let prog = Arc::new(Prog { bytes: AtomicU64::new(0), files: AtomicU64::new(0), total_bytes: total, total_files: items.len() as u64, stop: AtomicBool::new(false) });
    let rep = start_reporter(prog.clone(), title);
    let fails: Mutex<Vec<(usize, String)>> = Mutex::new(vec![]);
    run_pool(items.len(), |i| {
        let (src, rec) = &items[i];
        let dst = dst_root.join(&rec.rel);
        if let Ok(md) = fs::metadata(&dst) {
            if md.len() == rec.size && md.is_file() {
                if apply_attrs {
                    set_attrs(&dst, rec.attrs);
                }
                set_mtime(&dst, rec.mtime, rec.mtime_ns);
                prog.bytes.fetch_add(rec.size, Relaxed);
                prog.files.fetch_add(1, Relaxed);
                return;
            }
        }
        if let Some(parent) = dst.parent() {
            let _ = fs::create_dir_all(parent);
        }
        match copy_file(src, vol, rec.size, &dst, &prog) {
            Ok(()) => {
                if apply_attrs {
                    set_attrs(&dst, rec.attrs);
                }
                set_mtime(&dst, rec.mtime, rec.mtime_ns);
                prog.files.fetch_add(1, Relaxed);
            }
            Err(e) => fails.lock().unwrap().push((i, e.to_string())),
        }
    });
    prog.stop.store(true, Relaxed);
    let _ = rep.join();
    fails.into_inner().unwrap()
}

// ───────────────────────────── stage 1: backup ─────────────────────────────
fn stage_backup(c: &Ctx, s: &mut State) -> Result<(), String> {
    say("\n[1/4] BACKUP  drive -> C: (corrupted files are skipped)");
    let t0 = Instant::now();
    fs::create_dir_all(c.data()).map_err(|e| e.to_string())?;
    if c.force_raw && fs::read_dir(c.root()).is_ok() {
        let _ = ps(&format!("Write-VolumeCache -DriveLetter {}", c.letter));
    }
    let mounted = !c.force_raw && fs::read_dir(c.root()).is_ok();
    let mut files = vec![];
    let mut dirs = vec![];
    let mut skipped = vec![];
    let vol;
    if mounted {
        say("  mode: mounted volume (Windows filesystem)");
        vol = None;
        scan_mounted(&c.root(), &mut files, &mut dirs, &mut skipped);
    } else {
        say("  mode: RAW exFAT reader (volume is not mountable)");
        let v = exfat::Vol::open(s.disk, s.part_off).map_err(|e| format!("raw open failed (run as admin): {e}"))?;
        scan_raw(&v, v.root, 0, false, "", &mut files, &mut dirs, &mut skipped);
        vol = Some(v);
    }
    files.sort_by(|a, b| b.rec.size.cmp(&a.rec.size));
    let total: u64 = files.iter().map(|f| f.rec.size).sum();
    say(&format!("  found {} files, {} folders, {:.1} GB; {} unusable entries", files.len(), dirs.len(), gb(total), skipped.len()));
    let free = free_space(Path::new("C:\\"));
    let have = dir_size_hint(&c.data());
    if total.saturating_sub(have) + (1u64 << 30) > free {
        return Err(format!("not enough space on C: need {:.1} GB, free {:.1} GB", gb(total.saturating_sub(have)), gb(free)));
    }
    for d in &dirs {
        let _ = fs::create_dir_all(c.data().join(&d.rel));
    }
    let items: Vec<(Src, FileRec)> = files.iter().map(|f| (f.src.clone(), f.rec.clone())).collect();
    let fails = copy_all(&items, &c.data(), vol.as_ref(), false, "backup");
    let failed: HashSet<usize> = fails.iter().map(|f| f.0).collect();
    for (i, e) in &fails {
        skipped.push(format!("{} : {}", items[*i].1.rel, e));
    }
    let good: Vec<FileRec> = items.iter().enumerate().filter(|(i, _)| !failed.contains(i)).map(|(_, f)| f.1.clone()).collect();
    // verify the backup: every manifest file present with the right size
    say("  verifying backup...");
    let bad = Mutex::new(vec![]);
    run_pool(good.len(), |i| {
        let r = &good[i];
        match fs::metadata(c.data().join(&r.rel)) {
            Ok(m) if m.len() == r.size => {}
            _ => bad.lock().unwrap().push(r.rel.clone()),
        }
    });
    let bad = bad.into_inner().unwrap();
    if !bad.is_empty() {
        return Err(format!("backup verification failed for {} files, e.g. {}", bad.len(), bad[0]));
    }
    s.bytes = good.iter().map(|f| f.size).sum();
    s.files = good;
    s.dirs = dirs;
    s.skipped = skipped;
    s.backup_done = true;
    save(c, s)?;
    let _ = fs::write(c.work.join("skipped.txt"), s.skipped.join("\n"));
    say(&format!(
        "  backup OK: {} files, {:.1} GB in {:.1} min; {} skipped (see {})",
        s.files.len(),
        gb(s.bytes),
        t0.elapsed().as_secs_f64() / 60.0,
        s.skipped.len(),
        c.work.join("skipped.txt").display()
    ));
    Ok(())
}

fn dir_size_hint(p: &Path) -> u64 {
    walkdir::WalkDir::new(p).into_iter().filter_map(|e| e.ok()).filter_map(|e| e.metadata().ok()).filter(|m| m.is_file()).map(|m| m.len()).sum()
}

// ───────────────────────────── stage 2: format ─────────────────────────────
fn stage_format(c: &Ctx, s: &mut State) -> Result<(), String> {
    say("\n[2/4] FORMAT  repair the drive");
    if !s.backup_done {
        return Err("refusing to format: backup is not complete".into());
    }
    if !c.yes {
        say(&format!("  formatting {}: ({}, label '{}') in 10 s — backup is verified. Ctrl+C to abort.", c.letter, s.fs, s.label));
        for i in (1..=10).rev() {
            print!("\r  {i} ... ");
            let _ = io::stdout().flush();
            std::thread::sleep(Duration::from_secs(1));
        }
        println!();
    }
    let fsn = match s.fs.to_lowercase().as_str() {
        "ntfs" => "NTFS",
        "fat32" => "FAT32",
        _ => "exFAT",
    };
    let label = s.label.replace('\'', "''");
    let alloc = if s.cluster > 0 { format!(" -AllocationUnitSize {}", s.cluster) } else { String::new() };
    let l = c.letter;
    let r = ps(&format!("$ErrorActionPreference='Stop'; Format-Volume -DriveLetter {l} -FileSystem {fsn} -NewFileSystemLabel '{label}'{alloc} -Force -Confirm:$false | Out-Null"));
    if let Err(e) = r {
        say(&format!("  Format-Volume failed ({}), trying format.com", e.lines().next().unwrap_or("")));
        let a = if s.cluster > 0 { format!(" /A:{}", s.cluster) } else { String::new() };
        let o = Command::new("cmd")
            .args(["/c", &format!("format {l}: /FS:{fsn} /Q /Y /V:{}{a}", s.label)])
            .output()
            .map_err(|e| e.to_string())?;
        if !o.status.success() {
            return Err(format!("format failed: {}", String::from_utf8_lossy(&o.stdout)));
        }
    }
    wait_mounted(c, 30)?;
    s.format_done = true;
    save(c, s)?;
    say("  format OK — drive is healthy and empty");
    Ok(())
}

fn wait_mounted(c: &Ctx, secs: u64) -> Result<(), String> {
    for _ in 0..secs * 2 {
        if fs::read_dir(c.root()).is_ok() {
            return Ok(());
        }
        std::thread::sleep(Duration::from_millis(500));
    }
    Err(format!("{}: did not come online after format", c.letter))
}

// ───────────────────────────── stage 2b: ventoy ─────────────────────────────
fn ventoy_exe() -> Result<PathBuf, String> {
    let tools = PathBuf::from("C:\\DriveSwap\\tools\\ventoy");
    let find = || {
        walkdir::WalkDir::new(&tools).into_iter().filter_map(|e| e.ok()).find(|e| e.file_name().eq_ignore_ascii_case("Ventoy2Disk.exe")).map(|e| e.into_path())
    };
    if let Some(p) = find() {
        return Ok(p);
    }
    say("  downloading Ventoy (official GitHub release)...");
    fs::create_dir_all(&tools).map_err(|e| e.to_string())?;
    let zip = tools.join("ventoy-windows.zip");
    ps(&format!(
        "$ErrorActionPreference='Stop'; [Net.ServicePointManager]::SecurityProtocol=[Net.SecurityProtocolType]::Tls12; \
         $u='https://github.com/ventoy/Ventoy/releases/download/v1.1.18/ventoy-1.1.18-windows.zip'; \
         try {{ $r=Invoke-RestMethod 'https://api.github.com/repos/ventoy/Ventoy/releases/latest' -Headers @{{'User-Agent'='driveswap'}}; \
                $a=$r.assets | Where-Object {{ $_.name -like '*windows.zip' }} | Select-Object -First 1; if($a){{$u=$a.browser_download_url}} }} catch {{}}; \
         Invoke-WebRequest $u -OutFile '{z}' -UseBasicParsing; Expand-Archive '{z}' -DestinationPath '{t}' -Force",
        z = zip.display(),
        t = tools.display()
    ))?;
    find().ok_or_else(|| "Ventoy2Disk.exe not found after download".to_string())
}

fn stage_ventoy(c: &Ctx, s: &mut State) -> Result<(), String> {
    if !s.ventoy {
        s.ventoy_done = true;
        return Ok(());
    }
    say("\n[2b/4] VENTOY  make the drive a Ventoy boot drive again");
    let exe = ventoy_exe()?;
    let l = c.letter;
    let run = |mode: &str| -> (bool, String) {
        let o = Command::new(&exe).args(["VTOYCLI", mode, &format!("/Drive:{l}:"), "/NOUSBCheck"]).current_dir(exe.parent().unwrap()).output();
        match o {
            Ok(o) => (o.status.success(), format!("{}{}", String::from_utf8_lossy(&o.stdout), String::from_utf8_lossy(&o.stderr))),
            Err(e) => (false, e.to_string()),
        }
    };
    let (ok, out) = run("/U");
    if !ok {
        say(&format!("  update failed ({}), doing a fresh install", out.trim().lines().next().unwrap_or("")));
        let (ok2, out2) = run("/I");
        if !ok2 {
            return Err(format!("Ventoy install failed: {}", out2.trim()));
        }
    }
    // The install may re-create partitions: make sure the big partition is again <letter>: and labelled Ventoy
    std::thread::sleep(Duration::from_secs(3));
    let _ = ps(&format!(
        "$d={disk}; $p=Get-Partition -DiskNumber $d | Where-Object {{ $_.Type -ne 'Reserved' -and $_.Size -gt 1GB }} | Sort-Object Size -Descending | Select-Object -First 1; \
         if($p -and $p.DriveLetter -ne '{l}'){{ Set-Partition -DiskNumber $d -PartitionNumber $p.PartitionNumber -NewDriveLetter {l} }}; \
         Set-Volume -DriveLetter {l} -NewFileSystemLabel '{lab}' -ErrorAction SilentlyContinue",
        disk = s.disk,
        lab = s.label.replace('\'', "''")
    ));
    wait_mounted(c, 30)?;
    let chk = ps("(Get-Volume | Where-Object FileSystemLabel -eq 'VTOYEFI' | Measure-Object).Count").unwrap_or_default();
    if chk.trim() == "0" {
        return Err("Ventoy EFI partition (VTOYEFI) not found after install".into());
    }
    s.ventoy_done = true;
    save(c, s)?;
    say("  Ventoy OK");
    Ok(())
}

// ───────────────────────────── stage 3: restore ─────────────────────────────
fn hash_file(p: &Path) -> io::Result<[u8; 32]> {
    let mut f = File::open(p)?;
    let mut h = blake3::Hasher::new();
    let mut buf = vec![0u8; 8 << 20];
    loop {
        let n = f.read(&mut buf)?;
        if n == 0 {
            break;
        }
        h.update(&buf[..n]);
    }
    Ok(*h.finalize().as_bytes())
}

fn stage_restore(c: &Ctx, s: &mut State) -> Result<(), String> {
    say("\n[3/4] RESTORE  C: -> drive, exactly as it was");
    if !s.format_done {
        return Err("format stage not complete".into());
    }
    let t0 = Instant::now();
    let root = c.root();
    wait_mounted(c, 30)?;
    for d in &s.dirs {
        let _ = fs::create_dir_all(root.join(&d.rel));
    }
    let mut recs = s.files.clone();
    recs.sort_by(|a, b| b.size.cmp(&a.size));
    let items: Vec<(Src, FileRec)> = recs.into_iter().map(|r| (Src::Path(c.data().join(&r.rel)), r)).collect();
    let fails = copy_all(&items, &root, None, true, "restore");
    if !fails.is_empty() {
        return Err(format!("{} files failed to restore, e.g. {} : {}", fails.len(), items[fails[0].0].1.rel, fails[0].1));
    }
    // directory timestamps/attributes, deepest first
    let mut dirs = s.dirs.clone();
    dirs.sort_by_key(|d| std::cmp::Reverse(d.rel.matches('\\').count()));
    for d in &dirs {
        let p = root.join(&d.rel);
        if d.attrs & 0x27 != 0 {
            set_attrs(&p, d.attrs);
        }
        set_mtime(&p, d.mtime, d.mtime_ns);
    }
    say("  verifying restore...");
    let bad = Mutex::new(vec![]);
    run_pool(s.files.len(), |i| {
        let r = &s.files[i];
        let dst = root.join(&r.rel);
        let ok = match fs::metadata(&dst) {
            Ok(m) if m.len() == r.size => {
                let (mt, _) = m.modified().map(unix_of).unwrap_or((0, 0));
                (mt - r.mtime).abs() <= 2
            }
            _ => false,
        };
        let ok = ok && (!c.verify || r.size == 0 || hash_file(&dst).ok() == hash_file(&c.data().join(&r.rel)).ok());
        if !ok {
            bad.lock().unwrap().push(r.rel.clone());
        }
    });
    let bad = bad.into_inner().unwrap();
    if !bad.is_empty() {
        return Err(format!("restore verification failed for {} files, e.g. {}", bad.len(), bad[0]));
    }
    s.restore_done = true;
    save(c, s)?;
    say(&format!("  restore OK: {} files, {:.1} GB in {:.1} min", s.files.len(), gb(s.bytes), t0.elapsed().as_secs_f64() / 60.0));
    Ok(())
}

// ───────────────────────────── stage 4: cleanup ─────────────────────────────
fn stage_cleanup(c: &Ctx, s: &State) -> Result<(), String> {
    say("\n[4/4] CLEANUP  delete the backup from C:");
    if !s.restore_done {
        return Err("refusing to delete backup: restore not verified".into());
    }
    let before = free_space(Path::new("C:\\"));
    let all: Vec<PathBuf> = walkdir::WalkDir::new(&c.work).contents_first(true).into_iter().filter_map(|e| e.ok()).filter(|e| e.file_type().is_file()).map(|e| e.into_path()).collect();
    run_pool(all.len(), |i| {
        let _ = fs::remove_file(&all[i]);
    });
    let _ = fs::remove_dir_all(&c.work);
    let freed = free_space(Path::new("C:\\")).saturating_sub(before);
    say(&format!("  deleted backup folder, freed {:.1} GB on C:", gb(freed)));
    Ok(())
}

// ───────────────────────────── main ─────────────────────────────
fn usage() {
    println!(
        "driveswap — back up a drive to C:, format+fix it, reinstall Ventoy, restore everything, free the space\n\n\
         USAGE:  driveswap [DRIVE_LETTER] [options]\n\n\
         OPTIONS:\n  --yes          no 10-second countdown before the format\n  --verify       byte-hash verification after restore (slower)\n  \
         --raw          force the raw exFAT reader (use for unmountable drives)\n  --ventoy       always install Ventoy (default: only if the drive was Ventoy)\n  \
         --work DIR     backup folder (default C:\\DriveSwap\\<L>)\n  --stop-after S stop after stage S (backup|format|restore), for testing
  --status       show saved progress and exit\n  --reset        forget saved progress (does not touch any drive)\n\n\
         Re-running resumes at the next unfinished step."
    );
}

fn real_main() -> Result<(), String> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.iter().any(|a| a == "-h" || a == "--help") {
        usage();
        return Ok(());
    }
    let has = |f: &str| args.iter().any(|a| a == f);
    if !is_admin() {
        say("Administrator rights needed — relaunching elevated...");
        let exe = std::env::current_exe().map_err(|e| e.to_string())?;
        let mut a: Vec<String> = args.iter().map(|x| format!("'{}'", x.replace('\'', "''"))).collect();
        a.push("'--pause'".into());
        let script = format!("Start-Process -FilePath '{}' -ArgumentList {} -Verb RunAs -Wait", exe.display(), a.join(","));
        ps(&script)?;
        return Ok(());
    }
    let mut letter: Option<char> = None;
    let mut work: Option<PathBuf> = None;
    let mut i = 0;
    while i < args.len() {
        let a = &args[i];
        if a == "--work" {
            i += 1;
            work = args.get(i).map(PathBuf::from);
        } else if a == "--stop-after" {
            i += 1;
        } else if !a.starts_with("--") && letter.is_none() {
            letter = a.chars().next().map(|c| c.to_ascii_uppercase());
        }
        i += 1;
    }
    let letter = match letter {
        Some(l) => l,
        None => {
            print!("Drive letter to process [E]: ");
            let _ = io::stdout().flush();
            let mut s = String::new();
            let _ = io::stdin().read_line(&mut s);
            s.trim().chars().next().map(|c| c.to_ascii_uppercase()).unwrap_or('E')
        }
    };
    if !letter.is_ascii_alphabetic() {
        return Err("invalid drive letter".into());
    }
    let sysdrive = std::env::var("SystemDrive").unwrap_or("C:".into()).chars().next().unwrap_or('C');
    if letter == sysdrive.to_ascii_uppercase() || letter == 'C' {
        return Err("refusing to operate on the system drive".into());
    }
    let c = Ctx {
        letter,
        work: work.unwrap_or_else(|| PathBuf::from(format!("C:\\DriveSwap\\{}", letter))),
        yes: has("--yes"),
        verify: has("--verify"),
        force_raw: has("--raw"),
        ventoy_flag: has("--ventoy"),
    };
    if has("--fetch-ventoy") {
        let p = ventoy_exe()?;
        say(&format!("ventoy ready: {}", p.display()));
        return Ok(());
    }
    let stop_after: String = args.iter().position(|a| a == "--stop-after").and_then(|i| args.get(i + 1).cloned()).unwrap_or_default();
    if has("--reset") {
        let _ = fs::remove_file(c.state_path());
        say("saved progress cleared");
        return Ok(());
    }
    let mut s: State = match fs::read(c.state_path()) {
        Ok(b) => serde_json::from_slice(&b).map_err(|e| format!("corrupt state file {}: {e}", c.state_path().display()))?,
        Err(_) => drive_info(&c)?,
    };
    if s.drive != c.letter.to_string() {
        return Err("state belongs to another drive; use --reset or --work".into());
    }
    if has("--status") {
        println!(
            "drive {}: fs={} label='{}' backup={} format={} ventoy={} restore={} files={} bytes={:.1}GB skipped={}",
            s.drive, s.fs, s.label, s.backup_done, s.format_done, s.ventoy_done, s.restore_done, s.files.len(), gb(s.bytes), s.skipped.len()
        );
        return Ok(());
    }
    let t0 = Instant::now();
    say(&format!("driveswap: drive {}: fs={} label='{}' cluster={} ventoy={}", c.letter, s.fs, s.label, s.cluster, s.ventoy));
    if !s.backup_done {
        stage_backup(&c, &mut s)?;
        if stop_after == "backup" {
            return Ok(());
        }
    } else {
        say("\n[1/4] BACKUP  already done — skipping");
    }
    if !s.format_done {
        stage_format(&c, &mut s)?;
        if stop_after == "format" {
            return Ok(());
        }
    } else {
        say("\n[2/4] FORMAT  already done — skipping");
    }
    if !s.ventoy_done {
        if let Err(e) = stage_ventoy(&c, &mut s) {
            say(&format!("  WARNING: Ventoy step failed: {e}\n  continuing with the restore so no data is at risk; re-run installer manually if needed"));
            s.ventoy_done = true;
            save(&c, &s)?;
        }
    }
    if !s.restore_done {
        stage_restore(&c, &mut s)?;
        if stop_after == "restore" {
            return Ok(());
        }
    } else {
        say("\n[3/4] RESTORE  already done — skipping");
    }
    let skipped = s.skipped.clone();
    stage_cleanup(&c, &s)?;
    say(&format!("\nALL DONE in {:.1} min. {} corrupted/unreadable items were left out.", t0.elapsed().as_secs_f64() / 60.0, skipped.len()));
    if !skipped.is_empty() {
        let log = PathBuf::from(format!("C:\\DriveSwap\\skipped_{}.txt", c.letter));
        let _ = fs::create_dir_all("C:\\DriveSwap");
        let _ = fs::write(&log, skipped.join("\n"));
        say(&format!("  list of skipped items: {}", log.display()));
    }
    Ok(())
}

fn main() {
    let r = real_main();
    if let Err(e) = &r {
        eprintln!("\nERROR: {e}");
    }
    if std::env::args().any(|a| a == "--pause") {
        print!("\nPress Enter to close...");
        let _ = io::stdout().flush();
        let mut s = String::new();
        let _ = io::stdin().read_line(&mut s);
    }
    std::process::exit(if r.is_ok() { 0 } else { 1 });
}
