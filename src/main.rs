#![feature(portable_simd)]
#![feature(cold_path)]
#![feature(slice_split_once)]
#![feature(hasher_prefixfree_extras)]
#![feature(ptr_cast_array)]

use std::{
    borrow::Borrow,
    collections::{BTreeMap, HashMap, btree_map::Entry},
    fs::File,
    hash::{BuildHasher, Hash, Hasher},
    io::{self, prelude::*},
    path::Path,
    simd::{Simd, cmp::SimdPartialEq},
    sync::atomic::{AtomicU64, Ordering},
};

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
    let file_name = "measurements.txt";
    //let f = File::open("measurements.txt").unwrap();
    let mut stats = BTreeMap::new();
    let next_chunk = AtomicU64::new(0);
    std::thread::scope(|scope| {
        let nthreads = std::thread::available_parallelism().unwrap();
        let (tx, rx) = std::sync::mpsc::sync_channel(nthreads.get());
        for _ in 0..nthreads.get() {
            let tx = tx.clone();
            let next_chunk = &next_chunk;
            scope.spawn(move || tx.send(one(file_name, next_chunk)));
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
fn one(path: impl AsRef<Path>, next_chunk: &AtomicU64) -> HashMap<StrVec, Stat, FastHasherBuilder> {
    let mut stats = HashMap::with_capacity_and_hasher(1_024, FastHasherBuilder);
    let mut reader = ChunkReader::new(path).unwrap();

    // claim next chunk for this worker
    let get_next = || next_chunk.fetch_add(1, Ordering::Relaxed) as usize;

    while let Some(chunk) = reader.read_chunk(get_next()).unwrap() {
        let mut at = 0;
        while at < chunk.len() {
            let newline_at = at + unsafe { find_newline(&chunk[at..]).unwrap_unchecked() };
            let line = unsafe { chunk.get_unchecked(at..newline_at) };
            at = newline_at + 1;
            let (station, temperature) = unsafe { split_at_semicolon(line) };
            let t = parse_temperature(temperature);
            update_stats(&mut stats, station, t);
        }
    }
    stats
}

struct ChunkReader {
    file: File,
    file_len: u64,
    num_chunks: usize,
    buffer: Vec<u8>,
}

impl ChunkReader {
    // empirically set on my machine, tuneable
    const CHUNK_SIZE: usize = 1 << 16;
    // must be at least maximum line length
    const OVERLAP: usize = 106;

    fn new(path: impl AsRef<Path>) -> io::Result<Self> {
        // you would think this would be a classic use case for all threads to share a file
        // descriptor and use pread (via std::os::unix::fs::FileExt::read_exact_at), but it's
        // faster to open a new file descriptor in each thread. I guess it's a contention issue
        let file = File::open(path)?;
        let file_len = file.metadata().unwrap().len();
        Ok(Self {
            file,
            file_len,
            num_chunks: file_len.div_ceil(Self::CHUNK_SIZE as u64) as usize,
            buffer: vec![0; Self::CHUNK_SIZE + Self::OVERLAP],
        })
    }

    /// Reads the `chunk_index`th chunk from self.file
    fn read_chunk(&mut self, chunk_index: usize) -> io::Result<Option<&[u8]>> {
        if chunk_index >= self.num_chunks {
            return Ok(None);
        }
        let chunk_start = chunk_index as u64 * (Self::CHUNK_SIZE as u64);
        let chunk_size = std::cmp::min(self.buffer.len(), (self.file_len - chunk_start) as usize);

        self.file.seek(io::SeekFrom::Start(chunk_start))?;
        self.file.read_exact(&mut self.buffer[..chunk_size])?;
        Ok(Some(Self::trim_chunk(
            &self.buffer,
            chunk_index == 0,
            chunk_size,
        )))
    }

    // extracts chunk from self.buffer
    // for chunks after the first one, we seek to the first newline
    // for chunks besides the last one, we read forward after CHUNK_SIZE until
    // we find a newline
    // This contains plenty of branches but they're very predictable
    fn trim_chunk(chunk: &[u8], is_first_chunk: bool, chunk_size: usize) -> &[u8] {
        let start = if is_first_chunk {
            std::hint::cold_path();
            0
        } else {
            find_newline(chunk).unwrap() + 1
        };

        let end = if chunk_size < chunk.len() {
            // this occurs when this is the last chunk and chunk size doesn't evenly divide file
            // length
            std::hint::cold_path();
            chunk_size
        } else {
            find_newline(&chunk[Self::CHUNK_SIZE..]).unwrap() + Self::CHUNK_SIZE + 1
        };

        &chunk[start..end]
    }
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
