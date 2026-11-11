#![cfg(feature = "plugins")]
#![allow(clippy::unwrap_used, clippy::expect_used, reason = "integration tests")]

use wasmtime::{Cache, CacheConfig, Config, Engine, Instance, Module, Store};

#[test]
fn a_compiled_module_round_trips_through_the_disk_cache() {
    let directory = tempfile::tempdir().unwrap();
    let mut cache_config = CacheConfig::new();
    cache_config.with_directory(directory.path());
    let wasm = b"\0asm\x01\0\0\0\x01\x05\x01\x60\0\x01\x7f\x03\x02\x01\0\x07\x0a\x01\x06answer\0\0\x0a\x06\x01\x04\0\x41\x2a\x0b";

    for (hits, misses) in [(0, 1), (1, 0)] {
        let cache = Cache::new(cache_config.clone()).unwrap();
        let mut config = Config::new();
        config.cache(Some(cache.clone()));
        let engine = Engine::new(&config).unwrap();
        let module = Module::new(&engine, wasm).unwrap();
        let mut store = Store::new(&engine, ());
        let instance = Instance::new(&mut store, &module, &[]).unwrap();
        let answer = instance
            .get_typed_func::<(), i32>(&mut store, "answer")
            .unwrap();

        assert_eq!(answer.call(&mut store, ()).unwrap(), 42);
        assert_eq!(cache.cache_hits(), hits);
        assert_eq!(cache.cache_misses(), misses);
    }
}
