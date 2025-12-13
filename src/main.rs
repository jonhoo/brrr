#![feature(portable_simd)]
#![feature(cold_path)]
#![feature(slice_split_once)]
#![feature(hasher_prefixfree_extras)]
#![feature(ptr_cast_array)]

use std::io::Write;
use std::{
    borrow::Borrow,
    collections::{BTreeMap, HashMap, btree_map::Entry},
    fs::File,
    hash::{BuildHasher, Hash, Hasher},
    mem::ManuallyDrop,
    simd::{Simd, cmp::SimdPartialEq},
};

#[cfg(unix)]
use unix::*;

#[cfg(windows)]
use win::*;

const HASH_K: u64 = 0xf1357aea2e62a9c5;
const HASH_SEED: u64 = 0x13198a2e03707344;

struct FastHasherBuilder;
struct FastHasher(u64);

impl BuildHasher for FastHasherBuilder {
    type Hasher = FastHasher;

    fn build_hasher(&self) -> Self::Hasher {
        FastHasher(0)
    }
}

impl Hasher for FastHasher {
    fn finish(&self) -> u64 {
        self.0.rotate_left(26)
    }

    fn write_length_prefix(&mut self, _len: usize) {}

    fn write(&mut self, bytes: &[u8]) {
        let len = bytes.len();
        let mut acc = HASH_SEED;

        match len {
            0..4 => {
                let lo = bytes[0];
                let mid = bytes[len / 2];
                let hi = bytes[len - 1];
                acc ^= (lo as u64) | ((mid as u64) << 8) | ((hi as u64) << 16);
            }
            4.. => {
                acc ^= u32::from_le_bytes(bytes[0..4].try_into().unwrap()) as u64;
            }
        }

        self.0 = self.0.wrapping_add(acc).wrapping_mul(HASH_K);
    }
}

const INLINE: usize = std::mem::size_of::<AllocedStrVec>();
const LAST: usize = INLINE - 1;

#[repr(C)]
union StrVec {
    inlined: [u8; INLINE],
    heap: ManuallyDrop<AllocedStrVec>,
}

#[repr(C)]
struct AllocedStrVec {
    // if length high bit is set, then inlined into pointer then len
    // otherwise, pointer is a pointer to Vec<u8>
    ptr: *mut u8,
    // len must be last for alignment with `inlined[LAST]` in the `StrVec` union
    len: usize,
}

impl Drop for AllocedStrVec {
    fn drop(&mut self) {
        let len = usize::from_le(self.len);
        let ptr = self.ptr;
        let slice_ptr = std::ptr::slice_from_raw_parts_mut(ptr, len);
        let _ = unsafe { Box::from_raw(slice_ptr) };
    }
}

// SAFETY: effectively just a Vec<str>, which is fine across thread boundaries
unsafe impl Send for StrVec {}

impl StrVec {
    pub fn new(s: &[u8]) -> Self {
        if s.len() < INLINE {
            let mut combined = [0u8; INLINE];
            combined[..s.len()].copy_from_slice(s);
            combined[LAST] = s.len() as u8 + 1;
            Self { inlined: combined }
        } else {
            let ptr = Box::into_raw(s.to_vec().into_boxed_slice());
            Self {
                heap: ManuallyDrop::new(AllocedStrVec {
                    len: ptr.len().to_le(),
                    ptr: ptr.cast(),
                }),
            }
        }
    }
}

impl Drop for StrVec {
    fn drop(&mut self) {
        unsafe {
            if self.inlined[LAST] == 0x00 {
                ManuallyDrop::drop(&mut self.heap)
            }
        }
    }
}

impl AsRef<[u8]> for StrVec {
    fn as_ref(&self) -> &[u8] {
        unsafe {
            if self.inlined[LAST] != 0x00 {
                let len = self.inlined[LAST] as usize - 1;
                std::slice::from_raw_parts(self.inlined.as_ptr(), len)
            } else {
                std::hint::cold_path();
                let len = usize::from_le(self.heap.len);
                let ptr = self.heap.ptr;
                std::slice::from_raw_parts(ptr, len)
            }
        }
    }
}

impl PartialEq for StrVec {
    fn eq(&self, other: &Self) -> bool {
        unsafe {
            self.inlined[LAST] == other.inlined[LAST] && {
                std::hint::cold_path();
                self.as_ref() == other.as_ref()
            }
        }
    }
}

impl Eq for StrVec {}

impl Hash for StrVec {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.as_ref().hash(state)
    }
}

impl Borrow<[u8]> for StrVec {
    fn borrow(&self) -> &[u8] {
        self.as_ref()
    }
}

#[derive(Debug, Clone, Copy)]
struct Stat {
    min: i16,
    max: i16,
    sum: i64,
    count: u32,
}

impl Default for Stat {
    fn default() -> Self {
        Self {
            min: i16::MAX,
            sum: 0,
            count: 0,
            max: i16::MIN,
        }
    }
}

fn main() {
    let f = File::open("measurements.txt").unwrap();
    let mut stats = BTreeMap::new();
    std::thread::scope(|scope| {
        let map = Mmap::new(&f);
        let map = map.bytes;
        let nthreads = std::thread::available_parallelism().unwrap();
        let mut at = 0;
        let (tx, rx) = std::sync::mpsc::sync_channel(nthreads.get());
        let chunk_size = map.len() / nthreads;
        for _ in 0..nthreads.get() {
            let start = at;
            let end = (at + chunk_size).min(map.len());
            let end = if end == map.len() {
                map.len()
            } else {
                let newline_at = find_newline(&map[end..]).unwrap();
                end + newline_at + 1
            };
            let map = &map[start..end];
            at = end;
            let tx = tx.clone();
            scope.spawn(move || tx.send(one(map)));
        }

        drop(tx);
        for one_stat in rx {
            for (k, v) in one_stat {
                // SAFETY: the README promised
                match stats.entry(unsafe { String::from_utf8_unchecked(k.as_ref().to_vec()) }) {
                    Entry::Vacant(none) => {
                        none.insert(v);
                    }
                    Entry::Occupied(some) => {
                        let stat = some.into_mut();
                        stat.min = stat.min.min(v.min);
                        stat.sum += v.sum;
                        stat.count += v.count;
                        stat.max = stat.max.max(v.max);
                    }
                }
            }
        }
    });

    print(stats);
}

#[inline(never)]
fn print(stats: BTreeMap<String, Stat>) {
    let stdout = std::io::stdout();
    let stdout = stdout.lock();
    let mut writer = std::io::BufWriter::new(stdout);
    write!(writer, "{{").unwrap();
    let stats = BTreeMap::from_iter(
        stats
            .iter()
            // SAFETY: the README promised
            .map(|(k, v)| (unsafe { std::str::from_utf8_unchecked(k.as_ref()) }, *v)),
    );
    let mut stats = stats.into_iter().peekable();
    while let Some((station, stat)) = stats.next() {
        write!(
            writer,
            "{station}={:.1}/{:.1}/{:.1}",
            (stat.min as f64) / 10.,
            (stat.sum as f64) / 10. / (stat.count as f64),
            (stat.max as f64) / 10.
        )
        .unwrap();
        if stats.peek().is_some() {
            write!(writer, ", ").unwrap();
        }
    }
    write!(writer, "}}").unwrap();
}

#[inline(never)]
fn one(map: &[u8]) -> HashMap<StrVec, Stat, FastHasherBuilder> {
    let mut stats = HashMap::with_capacity_and_hasher(1_024, FastHasherBuilder);
    let mut at = 0;
    while at < map.len() {
        let newline_at = at + unsafe { find_newline(&map[at..]).unwrap_unchecked() };
        let line = unsafe { map.get_unchecked(at..newline_at) };
        at = newline_at + 1;
        let (station, temperature) = unsafe { split_at_semicolon(line) };
        let t = parse_temperature(temperature);
        update_stats(&mut stats, station, t);
    }
    stats
}

fn update_stats(stats: &mut HashMap<StrVec, Stat, FastHasherBuilder>, station: &[u8], t: i16) {
    let stats = match stats.get_mut(station) {
        Some(stats) => stats,
        None => stats.entry(StrVec::new(station)).or_default(),
    };
    if t < stats.min {
        stats.min = t;
    }
    if t > stats.max {
        stats.max = t;
    }
    stats.sum += i64::from(t);
    stats.count += 1;
}

// SAFETY: buffer must contain a semicolon in the last min(8, buffer.len()) bytes
unsafe fn split_at_semicolon(buffer: &[u8]) -> (&[u8], &[u8]) {
    let mut pos = buffer.len() - 4;
    unsafe {
        // SAFETY: readme promises there will be a semicolon
        while *buffer.get_unchecked(pos) != b';' {
            pos -= 1;
        }
        let (before, after) = buffer.split_at_unchecked(pos + 1);
        (&before[..before.len() - 1], after)
    }
}

pub fn find_newline(mut buffer: &[u8]) -> Option<usize> {
    const LANES: usize = 32;
    const SPLAT: Simd<u8, LANES> = Simd::splat(b'\n');

    let mut i = 0;
    while let Some((chunk, rest)) = buffer.split_first_chunk() {
        let bytes = Simd::<u8, LANES>::from_array(*chunk);
        let index = bytes.simd_eq(SPLAT).first_set().map(|set| set + i);
        if index.is_some() {
            return index;
        }
        i += LANES;
        buffer = rest;
    }

    let bytes = Simd::<u8, LANES>::load_or_default(buffer);
    bytes.simd_eq(SPLAT).first_set().map(|set| set + i)
}

#[inline]
fn parse_temperature(t: &[u8]) -> i16 {
    let tlen = t.len();
    unsafe { std::hint::assert_unchecked(tlen >= 3) };
    let is_neg = std::hint::select_unpredictable(t[0] == b'-', true, false);
    let sign = i16::from(!is_neg) * 2 - 1;
    let skip = usize::from(is_neg);
    let has_dd = std::hint::select_unpredictable(tlen - skip == 4, true, false);
    let mul = i16::from(has_dd) * 90 + 10;
    let t1 = mul * i16::from(t[skip] - b'0');
    let t2 = i16::from(has_dd) * 10 * i16::from(t[tlen - 3] - b'0');
    let t3 = i16::from(t[tlen - 1] - b'0');
    sign * (t1 + t2 + t3)
}

#[test]
fn pt() {
    assert_eq!(parse_temperature(b"0.0"), 0);
    assert_eq!(parse_temperature(b"9.2"), 92);
    assert_eq!(parse_temperature(b"-9.2"), -92);
    assert_eq!(parse_temperature(b"98.2"), 982);
    assert_eq!(parse_temperature(b"-98.2"), -982);
}

#[cfg(unix)]
mod unix {
    use super::File;
    use std::os::fd::AsRawFd;

    pub(crate) struct Mmap<'a> {
        pub(crate) bytes: &'a [u8],
    }

    impl<'a> Mmap<'a> {
        pub(crate) fn new(f: &'a File) -> Self {
            let len = f.metadata().unwrap().len();
            unsafe {
                let ptr = libc::mmap(
                    std::ptr::null_mut(),
                    len as libc::size_t,
                    libc::PROT_READ,
                    libc::MAP_PRIVATE,
                    f.as_raw_fd(),
                    0,
                );

                if ptr == libc::MAP_FAILED {
                    panic!("{:?}", std::io::Error::last_os_error());
                } else {
                    if libc::madvise(ptr, len as libc::size_t, libc::MADV_SEQUENTIAL) != 0 {
                        panic!("{:?}", std::io::Error::last_os_error())
                    }
                    Mmap {
                        bytes: std::slice::from_raw_parts(ptr as *const u8, len as usize),
                    }
                }
            }
        }
    }
}

#[cfg(windows)]
mod win {
    use super::File;
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::{
        Foundation::{CloseHandle, HANDLE},
        System::Memory::{
            CreateFileMappingW, FILE_MAP_READ, MEMORY_MAPPED_VIEW_ADDRESS, MapViewOfFile,
            PAGE_READONLY, UnmapViewOfFile,
        },
    };

    pub(crate) struct Mmap<'a> {
        mapping: HANDLE,
        view: MEMORY_MAPPED_VIEW_ADDRESS,
        pub(crate) bytes: &'a [u8],
    }

    impl Drop for Mmap<'_> {
        fn drop(&mut self) {
            unsafe {
                UnmapViewOfFile(self.view);
                CloseHandle(self.mapping);
            }
        }
    }

    impl<'a> Mmap<'a> {
        pub(crate) fn new(f: &'a File) -> Self {
            let len = f.metadata().unwrap().len();
            unsafe {
                let mapping = CreateFileMappingW(
                    f.as_raw_handle(),
                    std::ptr::null_mut(),
                    PAGE_READONLY,
                    0,
                    0,
                    std::ptr::null(),
                );

                if mapping.is_null() {
                    panic!("{:?}", std::io::Error::last_os_error());
                }

                let view = MapViewOfFile(mapping, FILE_MAP_READ, 0, 0, 0);

                if view.Value.is_null() {
                    CloseHandle(mapping);
                    panic!("{:?}", std::io::Error::last_os_error());
                }

                Mmap {
                    mapping,
                    view,
                    bytes: std::slice::from_raw_parts(view.Value as *const u8, len as usize),
                }
            }
        }
    }
}
