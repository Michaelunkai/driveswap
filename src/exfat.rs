//! Minimal read-only exFAT reader that works on the raw disk, so it can rescue
//! volumes Windows refuses to mount (e.g. corrupt up-case table).
use std::fs::{File, OpenOptions};
use std::io;
use std::os::windows::fs::{FileExt, OpenOptionsExt};

pub struct Vol {
    f: File,
    part_off: u64,
    fat: Vec<u32>,
    heap_off: u64,
    bps: u64,
    spc: u64,
    pub csz: u64,
    ccount: u32,
    pub root: u32,
}

#[derive(Clone, Debug)]
pub struct Ent {
    pub name: String,
    pub is_dir: bool,
    pub attr: u32,
    pub nofat: bool,
    pub fc: u32,
    pub size: u64,
    pub mtime: i64,
    pub mtime_ns: u32,
}

pub enum Item {
    Ok(Ent),
    Bad,
}

const AL: u64 = 4096;

impl Vol {
    pub fn open(disk: u32, part_off: u64) -> io::Result<Vol> {
        let f = OpenOptions::new()
            .read(true)
            .share_mode(3)
            .open(format!(r"\\.\PhysicalDrive{}", disk))?;
        let mut v = Vol { f, part_off, fat: vec![], heap_off: 0, bps: 512, spc: 1, csz: 512, ccount: 0, root: 0 };
        let mut bs = [0u8; 512];
        v.read_at(part_off, &mut bs)?;
        if &bs[3..11] != b"EXFAT   " {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "not an exFAT volume"));
        }
        let u32at = |o: usize| u32::from_le_bytes(bs[o..o + 4].try_into().unwrap());
        let fat_off = u32at(80) as u64;
        v.heap_off = u32at(88) as u64;
        v.ccount = u32at(92);
        v.root = u32at(96);
        v.bps = 1u64 << bs[108];
        v.spc = 1u64 << bs[109];
        v.csz = v.bps * v.spc;
        let mut raw = vec![0u8; (v.ccount as usize + 2) * 4];
        v.read_at(part_off + fat_off * v.bps, &mut raw)?;
        v.fat = raw.chunks_exact(4).map(|c| u32::from_le_bytes(c.try_into().unwrap())).collect();
        Ok(v)
    }

    /// Positioned read of arbitrary offset/length (sector aligned internally).
    pub fn read_at(&self, off: u64, buf: &mut [u8]) -> io::Result<()> {
        if off % AL == 0 && buf.len() as u64 % AL == 0 {
            return self.fill(off, buf);
        }
        let a = off & !(AL - 1);
        let e = (off + buf.len() as u64 + AL - 1) & !(AL - 1);
        let mut tmp = vec![0u8; (e - a) as usize];
        self.fill(a, &mut tmp)?;
        let s = (off - a) as usize;
        buf.copy_from_slice(&tmp[s..s + buf.len()]);
        Ok(())
    }

    fn fill(&self, mut off: u64, mut buf: &mut [u8]) -> io::Result<()> {
        while !buf.is_empty() {
            let n = self.f.seek_read(buf, off)?;
            if n == 0 {
                return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "short read"));
            }
            off += n as u64;
            buf = &mut buf[n..];
        }
        Ok(())
    }

    pub fn cluster_off(&self, c: u32) -> u64 {
        self.part_off + (self.heap_off + (c as u64 - 2) * self.spc) * self.bps
    }

    fn valid(&self, c: u32) -> bool {
        c >= 2 && (c as u64) < self.ccount as u64 + 2
    }

    /// Clusters for a file of `size` bytes. Err if the chain is damaged/too short.
    pub fn chain(&self, fc: u32, size: u64, nofat: bool) -> Result<Vec<u32>, String> {
        let n = ((size + self.csz - 1) / self.csz) as usize;
        if n == 0 {
            return Ok(vec![]);
        }
        if !self.valid(fc) {
            return Err("bad first cluster".into());
        }
        if nofat {
            if (fc as u64 + n as u64) > self.ccount as u64 + 2 {
                return Err("contiguous run out of range".into());
            }
            return Ok((fc..fc + n as u32).collect());
        }
        let mut out = Vec::with_capacity(n);
        let mut c = fc;
        while out.len() < n {
            if !self.valid(c) {
                return Err("chain too short".into());
            }
            out.push(c);
            c = self.fat[c as usize];
        }
        Ok(out)
    }

    fn dir_clusters(&self, fc: u32, size: u64, nofat: bool) -> Result<Vec<u32>, String> {
        if nofat {
            return self.chain(fc, size, true);
        }
        let mut out = vec![];
        let mut c = fc;
        while self.valid(c) {
            out.push(c);
            if out.len() > self.ccount as usize {
                return Err("directory loop".into());
            }
            c = self.fat[c as usize];
            if c >= 0xFFFF_FFF7 {
                break;
            }
        }
        if out.is_empty() {
            return Err("empty directory chain".into());
        }
        Ok(out)
    }

    pub fn read_dir(&self, fc: u32, size: u64, nofat: bool) -> Result<Vec<Item>, String> {
        let cl = self.dir_clusters(fc, size, nofat)?;
        let mut data = vec![0u8; cl.len() * self.csz as usize];
        let mut i = 0;
        while i < cl.len() {
            let mut j = i;
            while j + 1 < cl.len() && cl[j + 1] == cl[j] + 1 {
                j += 1;
            }
            let s = i * self.csz as usize;
            let e = (j + 1) * self.csz as usize;
            self.read_at(self.cluster_off(cl[i]), &mut data[s..e]).map_err(|e| e.to_string())?;
            i = j + 1;
        }
        Ok(parse_dir(&data))
    }

    /// Volume label from the root directory (exFAT entry 0x83).
    pub fn label(&self) -> String {
        if let Ok(cl) = self.dir_clusters(self.root, 0, false) {
            let mut d = vec![0u8; self.csz as usize];
            if self.read_at(self.cluster_off(cl[0]), &mut d).is_ok() {
                for e in d.chunks_exact(32) {
                    if e[0] == 0 {
                        break;
                    }
                    if e[0] == 0x83 {
                        let n = (e[1] as usize).min(11);
                        let u: Vec<u16> = e[2..2 + n * 2].chunks_exact(2).map(|c| u16::from_le_bytes([c[0], c[1]])).collect();
                        return String::from_utf16_lossy(&u);
                    }
                }
            }
        }
        String::new()
    }
}

fn checksum(es: &[u8]) -> u16 {
    let mut c: u16 = 0;
    for (i, &b) in es.iter().enumerate() {
        if i == 2 || i == 3 {
            continue;
        }
        c = (if c & 1 != 0 { 0x8000u16 } else { 0 }).wrapping_add(c >> 1).wrapping_add(b as u16);
    }
    c
}

fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146097 + doe - 719468
}

fn dos_to_unix(ts: u32, inc10: u8, tz: u8) -> (i64, u32) {
    let sec = ((ts & 0x1f) * 2) as i64;
    let min = ((ts >> 5) & 0x3f) as i64;
    let hour = ((ts >> 11) & 0x1f) as i64;
    let day = ((ts >> 16) & 0x1f) as i64;
    let mon = ((ts >> 21) & 0xf) as i64;
    let year = ((ts >> 25) & 0x7f) as i64 + 1980;
    if day == 0 || mon == 0 || mon > 12 {
        return (315532800, 0);
    }
    let mut t = days_from_civil(year, mon, day) * 86400 + hour * 3600 + min * 60 + sec;
    let mut ns = 0;
    if inc10 <= 199 {
        t += (inc10 / 100) as i64;
        ns = (inc10 % 100) as u32 * 10_000_000;
    }
    if tz & 0x80 != 0 {
        let off = (((tz << 1) as i8) >> 1) as i64;
        t -= off * 900;
    }
    (t, ns)
}

fn parse_dir(data: &[u8]) -> Vec<Item> {
    let mut res = vec![];
    let mut i = 0;
    while i + 32 <= data.len() {
        let t = data[i];
        if t == 0 {
            break;
        }
        if t != 0x85 {
            i += 32;
            continue;
        }
        let sc = data[i + 1] as usize;
        let end = i + 32 * (sc + 1);
        if sc < 2 || end > data.len() {
            res.push(Item::Bad);
            i += 32;
            continue;
        }
        let es = &data[i..end];
        i = end;
        if u16::from_le_bytes([es[2], es[3]]) != checksum(es) || es[32] != 0xC0 {
            res.push(Item::Bad);
            continue;
        }
        let attr = u16::from_le_bytes([es[4], es[5]]) as u32;
        let ts = u32::from_le_bytes(es[12..16].try_into().unwrap());
        let (mtime, mtime_ns) = dos_to_unix(ts, es[21], es[24]);
        let nl = es[35] as usize;
        let nofat = es[33] & 2 != 0;
        let fc = u32::from_le_bytes(es[52..56].try_into().unwrap());
        let size = u64::from_le_bytes(es[56..64].try_into().unwrap());
        let mut nm: Vec<u16> = vec![];
        for k in 2..=sc {
            let e = &es[32 * k..32 * k + 32];
            if e[0] == 0xC1 {
                nm.extend(e[2..32].chunks_exact(2).map(|c| u16::from_le_bytes([c[0], c[1]])));
            }
        }
        if nm.len() < nl || nl == 0 {
            res.push(Item::Bad);
            continue;
        }
        let name = String::from_utf16_lossy(&nm[..nl]);
        res.push(Item::Ok(Ent { name, is_dir: attr & 0x10 != 0, attr, nofat, fc, size, mtime, mtime_ns }));
    }
    res
}
