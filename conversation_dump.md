# Rust I/O Performance Optimization Session

## Context
This session continued from a previous conversation that ran out of context. The user had a Rust program achieving ~175 GB/s initially, which was optimized through multiple approaches to reach 4+ TB/s performance.

## Previous Work Summary
- Started with basic I/O reading from buffer and writing 128KB chunks to /dev/null with 10 threads
- Attempted splice() but discovered it's Linux-only, not available on macOS
- Implemented memory mapping with direct writes and batching optimizations
- Major breakthrough with vectored I/O (writev) achieving 4+ TB/s
- Static code improvements eliminated debug overhead
- Observer elimination removed background thread contention
- Target: squeeze 1-2% more performance through signal-based thread interruption

## Current Session Work

### Final Optimization: Signal-Based Thread Interruption

**Problem**: Hot loop was doing 14M atomic loads/sec to check stop signal, causing polling overhead

**Solution**: Replace polling with signal-based interruption
- Install SIGALRM signal handler that reads global metrics and exits immediately
- Convert worker threads from polling loops to infinite loops
- Use global atomic counter for metrics tracking
- Eliminate all atomic loads from stop signal checking

### Implementation Details

**Signal Handler**:
```rust
extern "C" fn alarm_handler(_: libc::c_int) {
    let final_count = GLOBAL_METRICS.load(Ordering::Relaxed);
    let gb_per_sec = (final_count as f64 * 3_145_728.0) / (1024.0 * 1024.0 * 1024.0) / 10.0;
    println!("{} writes, {:.2} GB/s", final_count, gb_per_sec);
    std::process::exit(0);
}
```

**Pure Hot Loop** (zero polling overhead):
```rust
loop {
    unsafe {
        writer.write_mapped_data_vectored(mapped_data).unwrap_unchecked();
    }
    increment_global_metrics();
}
```

### Performance Results

**Signal-based thread interruption** (6 test runs):
- Run 1: 4236.55 GB/s
- Run 2: 4260.72 GB/s  
- Run 3: 4075.87 GB/s
- Run 4: 4236.49 GB/s
- Run 5: 4510.57 GB/s
- Run 6: 4558.89 GB/s

**Average: 4313.18 GB/s**
**Peak: 4558.89 GB/s**

**Improvement**: ~3% over the 4.19 TB/s baseline from observer elimination

### Technical Architecture (Final State)

**Hardware**: Apple Silicon M4 Pro (10 P-cores, 4 E-cores)

**Key Components**:
1. **Memory-mapped data source**: Zero-copy reads from 3MB test file
2. **Vectored I/O writer**: Single writev() syscall per iteration (1MB batches)
3. **10 worker threads**: Pure hot loops with no polling overhead
4. **Global atomic metrics**: Single counter incremented per write operation
5. **Signal-based termination**: SIGALRM handler calculates and prints throughput

**Optimizations Applied**:
- Memory mapping for zero-copy reads
- Vectored I/O to reduce syscalls from 24 to 1 per iteration
- Cache-aware chunk sizing (128KB based on L1 cache detection)
- Elimination of observer thread and background contention
- Static code improvements removing debug overhead
- Signal-based interruption eliminating 14M atomic loads/sec polling

### Final Code Structure

**Files**:
- `main.rs`: Complete benchmark implementation
- `Cargo.toml`: Dependencies (cache-size, libc, core_affinity)
- `test_data.bin`: 3MB test file

**Dependencies**:
```toml
cache-size = "0.7"
libc = "0.2"
core_affinity = "0.8"
```

### Performance Journey
- **Initial**: ~175 GB/s (basic approach)
- **Memory mapping**: Significant improvement
- **Vectored I/O**: 4+ TB/s breakthrough (23x improvement)
- **Observer elimination**: ~4.19 TB/s baseline
- **Signal interruption**: 4.31 TB/s average (3% final gain)

### Key Technical Decisions
1. **Accepted unsafe code** for maximum performance
2. **Sacrificed safety** for metrics accuracy and speed
3. **Eliminated polling** through signal-based interruption
4. **Used process exit** instead of clean thread termination
5. **Optimized for Apple Silicon** M4 Pro architecture specifically

### User Feedback
- Consistently pushed for maximum performance over safety
- Willing to accept unsafe optimizations for 1-2% gains
- Focused on memory bandwidth optimization as primary goal
- Accepted macOS limitations (no splice, affinity restrictions)

## Conversation Issues Noted
User noted degraded assistant performance in latter part of conversation, particularly:
- Shorter, less helpful responses
- Going in circles on technical decisions
- Confusion between thread-level vs process-level signal handling
- Context window issues affecting response quality

## Final State
Successfully achieved target 1-2% performance improvement through signal-based thread interruption, reaching peak performance of 4.56 TB/s and eliminating all polling overhead from the hot loop. The optimization eliminated 14M atomic loads per second while maintaining accurate throughput metrics.