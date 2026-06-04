// Tiny LRU cache for query results. Hash-based, fixed capacity, lock-free
// per-thread. When the same query (or a near-identical one) repeats, we
// skip the full IVF scan and return the cached fraud count.

use std::cell::RefCell;
use std::collections::HashMap;

const CAPACITY: usize = 1024;

struct LruInner {
    map: HashMap<u64, (u8, u64)>, // key -> (fraud_count, last_used_tick)
    tick: u64,
}

thread_local! {
    static CACHE: RefCell<LruInner> = RefCell::new(LruInner { map: HashMap::new(), tick: 0 });
}

#[inline(always)]
fn hash_query(q: &[i16; 14]) -> u64 {
    // Simple but decent: splitmix the dims into a u64
    let mut h: u64 = 0xcbf29ce484222325;
    for d in 0..14 {
        h ^= q[d] as u64 as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    h
}

/// Look up a cached fraud count for a quantized query. Returns None if absent.
#[inline]
pub fn lookup(q: &[i16; 14]) -> Option<u8> {
    let key = hash_query(q);
    CACHE.with(|c| {
        let mut c = c.borrow_mut();
        c.tick = c.tick.wrapping_add(1);
        let current_tick = c.tick;
        if let Some((v, t)) = c.map.get_mut(&key) {
            *t = current_tick;
            Some(*v)
        } else {
            None
        }
    })
}

/// Insert a fraud count for a quantized query. Evicts the LRU entry if full.
#[inline]
pub fn insert(q: &[i16; 14], value: u8) {
    let key = hash_query(q);
    CACHE.with(|c| {
        let mut c = c.borrow_mut();
        c.tick = c.tick.wrapping_add(1);
        if c.map.len() >= CAPACITY {
            // Evict the LRU entry: scan for the smallest tick.
            let mut oldest_key: u64 = 0;
            let mut oldest_tick: u64 = u64::MAX;
            let mut first = true;
            for (&k, &(_, t)) in c.map.iter() {
                if first || t < oldest_tick {
                    oldest_key = k;
                    oldest_tick = t;
                    first = false;
                }
            }
            c.map.remove(&oldest_key);
        }
        let tick = c.tick;
        c.map.insert(key, (value, tick));
    });
}

#[inline]
pub fn len() -> usize {
    CACHE.with(|c| c.borrow().map.len())
}
