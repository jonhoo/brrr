#![feature(portable_simd)]
#![feature(cold_path)]
#![feature(slice_split_once)]
#![feature(hasher_prefixfree_extras)]
#![feature(ptr_cast_array)]

use std::io::Write;
use std::{
    borrow::Borrow,
    collections::{BTreeMap, HashMap, btree_map::Entry},
    ffi::{c_int, c_void},
    fs::File,
    hash::{BuildHasher, Hash, Hasher},
    os::fd::AsRawFd,
    simd::{cmp::SimdPartialEq, u8x64},
};

const SEMI: u8x64 = u8x64::splat(b';');
const NEWL: u8x64 = u8x64::splat(b'\n');

struct FastHasherBuilder;
struct FastHasher(u64);

impl BuildHasher for FastHasherBuilder {
    type Hasher = FastHasher;

    fn build_hasher(&self) -> Self::Hasher {
        FastHasher(0xcbf29ce484222325)
    }
}

impl Hasher for FastHasher {
    fn finish(&self) -> u64 {
        self.0 ^ self.0.rotate_right(33) ^ self.0.rotate_right(15)
    }

    fn write_length_prefix(&mut self, _len: usize) {}

    fn write(&mut self, bytes: &[u8]) {
        let mut word = [0u64; 2];
        unsafe {
            std::ptr::copy(
                bytes.as_ptr(),
                word.as_mut_ptr().cast::<u8>(),
                bytes.len().min(16),
            )
        };
        self.0 = word[0] ^ word[1];
    }
}

const INLINE: usize = 16;
const LAST: usize = INLINE - 1;

union StrVec {
    inlined: [u8; INLINE],
    // if length high bit is set, then inlined into pointer then len
    // otherwise, pointer is a pointer to Vec<u8>
    heap: (usize, *mut u8),
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
                heap: (ptr.len().to_be(), ptr as *mut u8),
            }
        }
    }
}

impl Drop for StrVec {
    fn drop(&mut self) {
        if unsafe { self.inlined[LAST] } == 0x00 {
            unsafe {
                let len = usize::from_be(self.heap.0);
                let ptr = self.heap.1;
                let slice_ptr = std::ptr::slice_from_raw_parts_mut(ptr, len);
                let _ = Box::from_raw(slice_ptr);
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
                let len = usize::from_be(self.heap.0);
                let ptr = self.heap.1;
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
        let map = mmap(&f);
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
                let newline_at = next_newline(&map[end..], 0);
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
    let mut stats: [HashMap<StrVec, Stat, FastHasherBuilder>; 2] = [
        HashMap::with_capacity_and_hasher(1024 / 2, FastHasherBuilder),
        HashMap::with_capacity_and_hasher(1024 / 2, FastHasherBuilder),
    ];

    let mut at = 0;
    while at < map.len() {
        let newline_at = at + next_newline(map, at);
        let line = unsafe { map.get_unchecked(at..newline_at) };
        at = newline_at + 1;
        let semi = semi_at(line);
        let station = unsafe { line.get_unchecked(..semi) };
        let temperature = unsafe { line.get_unchecked(semi + 1..) };
        let t = parse_temperature(temperature);
        let bucket = unsafe { stats.get_unchecked_mut(station.len() & 1) };

        update_stats(bucket, station, t);
    }

    let [mut s0, mut s1] = stats;
    s0.extend(s1.drain());
    s0
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

#[inline]
fn next_newline(map: &[u8], at: usize) -> usize {
    let rest = unsafe { map.get_unchecked(at..) };
    let against = if let Some(restu8x64) = rest.first_chunk::<64>() {
        u8x64::from_array(*restu8x64)
    } else {
        std::hint::cold_path();
        u8x64::load_or_default(rest)
    };
    let newline_eq = NEWL.simd_eq(against);
    if let Some(i) = newline_eq.first_set() {
        i
    } else {
        // we know, line is at most 100+1+5 = 106b,
        // but we can only search 64b, so the search _may_ have to fall back to memchr
        // we know there _must_ be a newline, so rest[64..] must be non-empty
        std::hint::cold_path();
        let restrest = unsafe { rest.get_unchecked(64..) };
        // SAFETY: restrest is valid for at least restrest.len() bytes
        let next_newline = unsafe {
            libc::memchr(
                restrest.as_ptr() as *const c_void,
                b'\n' as c_int,
                restrest.len(),
            )
        };
        assert!(!next_newline.is_null());
        // SAFETY: memchr always returns pointers in restrest, which are valid
        let len = unsafe { (next_newline as *const u8).offset_from(restrest.as_ptr()) } as usize;
        64 + len
    }
}

#[inline]
fn semi_at(line: &[u8]) -> usize {
    // we know, line is at most 100+1+5 = 106b
    if line.len() > 64 {
        std::hint::cold_path();
        line.iter().position(|c| *c == b';').unwrap()
    } else {
        let delim_eq = SEMI.simd_eq(u8x64::load_or_default(line));
        // SAFETY: we're promised there is a ; in every line
        unsafe { delim_eq.first_set().unwrap_unchecked() }
    }
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

fn mmap(f: &File) -> &'_ [u8] {
    let len = f.metadata().unwrap().len();
    unsafe {
        let ptr = libc::mmap(
            std::ptr::null_mut(),
            len as libc::size_t,
            libc::PROT_READ,
            libc::MAP_SHARED,
            f.as_raw_fd(),
            0,
        );

        if ptr == libc::MAP_FAILED {
            panic!("{:?}", std::io::Error::last_os_error());
        } else {
            if libc::madvise(ptr, len as libc::size_t, libc::MADV_SEQUENTIAL) != 0 {
                panic!("{:?}", std::io::Error::last_os_error())
            }
            std::slice::from_raw_parts(ptr as *const u8, len as usize)
        }
    }
}
