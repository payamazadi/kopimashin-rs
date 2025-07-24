use std::fs::{File, OpenOptions};
use std::io::{Result as IoResult, Write};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};
use std::os::unix::io::AsRawFd;

// Global state for metrics tracking
static GLOBAL_METRICS: AtomicU64 = AtomicU64::new(0);

// Apple Silicon architecture detection
fn detect_apple_silicon_specs() -> Option<(usize, usize, usize)> {
    // M4 Pro: 10 P-cores + 4 E-cores, 128KB L1 data per P-core, 12MB shared L2
    #[cfg(all(target_arch = "aarch64", target_os = "macos"))]
    {
        Some((128 * 1024, 12 * 1024 * 1024, 10)) // L1 data per P-core, L2, P-core count
    }
    #[cfg(not(all(target_arch = "aarch64", target_os = "macos")))]
    {
        None
    }
}

// Detect optimal thread count based on CPU architecture
fn detect_optimal_thread_count() -> usize {
    if let Some((_, _, p_cores)) = detect_apple_silicon_specs() {
        // For memory-intensive workloads on Apple Silicon:
        // Use P-cores only - they have larger caches and better performance
        p_cores
    } else {
        // Default to 10 threads (M4 Pro P-core count) for consistency
        10
    }
}

// Cache-aware chunk size detection using L1/L2 hierarchy
fn detect_optimal_chunk_size() -> usize {
    // Try platform-specific detection first
    let (l1_data_size, l2_size) = if let Some((l1, l2, _)) = detect_apple_silicon_specs() {
        (Some(l1), Some(l2))
    } else {
        (cache_size::l1_cache_size(), cache_size::l2_cache_size())
    };
    
    let l1_data_size = l1_data_size.unwrap_or(32 * 1024); // 32KB fallback
    let l2_size = l2_size.unwrap_or(512 * 1024); // 512KB fallback
    
    // Minimum: L1 data cache size (ensures we utilize L1 fully)
    let min_chunk = l1_data_size;
    
    // Maximum: L2 cache size / 2 (leaves room for other data)
    let max_chunk = l2_size / 2;
    
    // Target: Sweet spot for I/O performance (64-128KB range)
    let target_chunk = 128 * 1024;
    
    // Choose the best size within our constraints
    target_chunk.clamp(min_chunk, max_chunk)
}

// Memory-mapped data source - zero-copy shared kernel pages
struct MappedDataSource {
    data: *const u8,
    size: usize,
    _file: File, // Keep file handle alive
}

unsafe impl Send for MappedDataSource {}
unsafe impl Sync for MappedDataSource {}

impl MappedDataSource {
    fn from_file(path: &str) -> IoResult<Self> {
        let file = File::open(path)?;
        let metadata = file.metadata()?;
        let size = metadata.len() as usize;
        
        // Memory map the file (read-only, shared)
        let data = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                size,
                libc::PROT_READ,
                libc::MAP_PRIVATE,
                file.as_raw_fd(),
                0,
            )
        };
        
        if data == libc::MAP_FAILED {
            return Err(std::io::Error::last_os_error());
        }
        
        // Pre-fault pages manually (macOS doesn't have MAP_POPULATE)
        let page_size = unsafe { libc::sysconf(libc::_SC_PAGESIZE) } as usize;
        unsafe {
            let mut i = 0;
            while i < size {
                // Touch each page to force it into memory
                let _touch: u8 = std::ptr::read_volatile((data as *const u8).add(i));
                i += page_size;
            }
            
            // Multiple memory access hints for optimal performance
            libc::madvise(data as *mut libc::c_void, size, libc::MADV_SEQUENTIAL);
            libc::madvise(data as *mut libc::c_void, size, libc::MADV_WILLNEED);
            // Note: MADV_HUGEPAGE not available on macOS
        }
        
        Ok(Self {
            data: data as *const u8,
            size,
            _file: file,
        })
    }

    fn data(&self) -> &[u8] {
        unsafe { std::slice::from_raw_parts(self.data, self.size) }
    }

    fn size(&self) -> usize {
        self.size
    }
}

impl Drop for MappedDataSource {
    fn drop(&mut self) {
        unsafe {
            libc::munmap(self.data as *mut libc::c_void, self.size);
        }
    }
}

// Configuration - pure data, no behavior
#[derive(Clone)]
struct Config {
    thread_count: usize,
    source_file: String,
    chunk_size: usize,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            thread_count: detect_optimal_thread_count(),
            source_file: "test_data.bin".to_string(),
            chunk_size: detect_optimal_chunk_size(),
        }
    }
}

// Global metrics increment - updates shared atomic counter
fn increment_global_metrics() {
    GLOBAL_METRICS.fetch_add(1, Ordering::Relaxed);
}

// Compile-time constant for optimal batch size
const BATCH_SIZE: usize = 1024 * 1024; // 1MB

// Direct memory-mapped writer with batched writes
struct MappedWriter {
    dest_file: File,
    io_vectors: Vec<std::io::IoSlice<'static>>, // Pre-allocated, reusable
}

impl MappedWriter {
    fn new() -> IoResult<Self> {
        let dest_file = OpenOptions::new()
            .write(true)
            .open("/dev/null")?;
            
        // Pre-allocate IoSlice vector (3MB / 1MB = 3 chunks max)
        let io_vectors = Vec::with_capacity(3);
            
        Ok(Self {
            dest_file,
            io_vectors,
        })
    }

    fn write_mapped_data_vectored(&mut self, mapped_data: &[u8]) -> IoResult<()> {
        // Reuse pre-allocated vector to avoid heap allocations
        self.io_vectors.clear();
        
        // Build IoSlice vector using compile-time batch size
        for chunk in mapped_data.chunks(BATCH_SIZE) {
            // SAFETY: IoSlice lifetime matches mapped_data which outlives this call
            unsafe {
                self.io_vectors.push(std::io::IoSlice::new(std::mem::transmute(chunk)));
            }
        }
        
        // Single writev syscall - no error handling overhead in hot path
        use std::io::Write;
        unsafe {
            self.dest_file.write_vectored(&self.io_vectors).unwrap_unchecked()
        };
        
        Ok(())
    }
}

// Eliminated MeasurementObserver - calculate final stats in main thread instead

// Worker factory - creates and runs workers
struct WorkerPool;

impl WorkerPool {
    fn spawn_writers(
        config: &Config,
        data_source: Arc<MappedDataSource>,
    ) -> Vec<thread::JoinHandle<()>> {
        (0..config.thread_count)
            .map(|_| {
                let thread_data_source = Arc::clone(&data_source);
                
                thread::spawn(move || {
                    // Create writer without dynamic batch size parameter
                    let mut writer = unsafe { 
                        MappedWriter::new().unwrap_unchecked()
                    };
                    
                    let mapped_data = thread_data_source.data();
                    
                    // Pure hot loop - zero polling overhead, batched metrics
                    loop {
                        unsafe {
                            writer.write_mapped_data_vectored(mapped_data).unwrap_unchecked();
                        }
                        increment_global_metrics();
                    }
                })
            })
            .collect()
    }
}

fn main() {
    let config = Config::default();
    
    // Memory-map source data (shared kernel pages, zero-copy reads)
    println!("Memory-mapping source data: {}", config.source_file);
    let data_source = Arc::new(MappedDataSource::from_file(&config.source_file)
        .expect("Failed to memory-map source file"));
    
    // Display cache detection info
    let l1_size = cache_size::l1_cache_size();
    let l2_size = cache_size::l2_cache_size();
    
    match (l1_size, l2_size) {
        (Some(l1), Some(l2)) => {
            println!("Detected L1: {} KB, L2: {} KB → chunk size: {} KB", 
                     l1 / 1024, l2 / 1024, config.chunk_size / 1024);
        },
        (Some(l1), None) => {
            println!("Detected L1: {} KB, L2: unknown → chunk size: {} KB", 
                     l1 / 1024, config.chunk_size / 1024);
        },
        (None, Some(l2)) => {
            println!("Detected L1: unknown, L2: {} KB → chunk size: {} KB", 
                     l2 / 1024, config.chunk_size / 1024);
        },
        (None, None) => {
            println!("Cache detection failed (non-x86 CPU) → using default chunk size: {} KB", 
                     config.chunk_size / 1024);
        }
    }
    
    println!("Memory-mapped zero-copy test: {} -> /dev/null", config.source_file);
    println!("Buffer: {} MB, Chunk: {} KB, Threads: {} (continuous mode)",
             data_source.size() / 1024 / 1024,
             config.chunk_size / 1024,
             config.thread_count);

    // Spawn workers (they run infinite loops)
    let _worker_handles = WorkerPool::spawn_writers(&config, Arc::clone(&data_source));
    
    // Monitoring thread for throughput calculation and display
    let file_size = data_source.size();
    let mut last_count = 0u64;
    let mut last_time = Instant::now();
    
    println!("Starting continuous throughput monitoring (updates every 3s)...");
    
    loop {
        thread::sleep(Duration::from_secs(3));
        
        let current_count = GLOBAL_METRICS.load(Ordering::Relaxed);
        let current_time = Instant::now();
        let elapsed = current_time.duration_since(last_time).as_secs_f64();
        
        if elapsed > 0.0 {
            let delta_count = current_count - last_count;
            let writes_per_sec = delta_count as f64 / elapsed;
            let gb_per_sec = (delta_count as f64 * file_size as f64) / (1024.0 * 1024.0 * 1024.0) / elapsed;
            
            // Update display in-place using carriage return
            print!("\r{} total writes, {:.0} writes/sec, {:.2} GB/s          ", 
                   current_count, writes_per_sec, gb_per_sec);
            std::io::stdout().flush().unwrap();
            
            last_count = current_count;
            last_time = current_time;
        }
    }
}
